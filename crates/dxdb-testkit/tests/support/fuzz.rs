//! A differential fuzz harness: random operation sequences, checked against invariants.
//!
//! The conformance suite checks what upstream thought to test. The fixtures check what we
//! thought to add. This checks **invariants we can state but not enumerate**, which is where
//! a storage that passes 49/49 conformance can still lose a write under a sequence nobody
//! wrote down.
//!
//! # Why invariants rather than recorded expectations
//!
//! A recorded expectation ("after these writes, query returns these ids") is one sequence.
//! An invariant ("`count` always equals `query().len()`") is every sequence. The second is
//! what a random test can check, because it needs no oracle: it compares the storage
//! against *itself* through two different code paths.
//!
//! The invariants here are the four PLAN.md 7.3 names, plus two that the trait docs imply:
//!
//! | invariant | what it catches |
//! | --- | --- |
//! | per-document atomicity | a bulk write that rolls back a successful row, or applies a failed one |
//! | monotonic revisions | a write that moves `_rev` backwards, or reuses a height |
//! | no lost writes | a successful write that a later read does not see |
//! | `count == query().len()` | a count path that disagrees with the query path |
//! | `find_documents_by_id` agrees with `query` | a by-id path that disagrees with the scan |
//! | delete/resurrect round-trips | a tombstone that cannot be updated back to live |
//!
//! # Differential
//!
//! Every storage runs the **same seeds**. A divergence between two storages on one seed is
//! itself a finding, and is reported as one: it means at least one of them is wrong, and
//! the seed reproduces it. That is stronger than each storage being checked only against
//! invariants, because two storages can each satisfy every invariant and still disagree
//! about observable behaviour.
//!
//! # Reproducing a failure
//!
//! Every assertion carries the seed and the operation log. Re-run one seed with:
//!
//! ```text
//! DXDB_FUZZ_SEED=<seed> DXDB_FUZZ_SEEDS=1 cargo test -p dxdb-testkit --test differential_fuzz
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use dxdb_storage::{
    BulkWriteRow, DocumentData, DocumentWriteData, DynRxStorageInstance, Operation, RxStorage,
    RxStorageInstance, StorageSchema, WriteError,
};
use serde_json::{json, Map, Value};

use dxdb_testkit::harness::{fill_with_default_settings, InstanceSpec};
use dxdb_testkit::util;

// ---------------------------------------------------------------------------
// The operation alphabet
// ---------------------------------------------------------------------------

/// One operation in a random sequence.
///
/// The alphabet is deliberately small and weighted toward the operations that interact:
/// an insert that conflicts, an update from a stale `previous`, a delete of something that
/// was already deleted, a bulk write mixing valid rows with a conflicting one. Those
/// interactions are where per-document atomicity and revision monotonicity actually get
/// stressed; a sequence of happy-path inserts tests almost nothing.
#[derive(Clone, Debug)]
pub enum Op {
    /// Insert a fresh document.
    Insert { id: String, value: i64 },
    /// Insert over whatever is there, with no `previous`. Must conflict iff a doc exists.
    InsertOverExisting { id: String, value: i64 },
    /// Update a document the harness believes exists, from its believed state.
    Update { id: String, value: i64 },
    /// Update from a deliberately **stale** previous revision. Must always conflict.
    StaleUpdate { id: String, value: i64 },
    /// Delete a document the harness believes is live.
    Delete { id: String },
    /// Delete something already deleted. Must be accepted (a tombstone is a document).
    DeleteTwice { id: String },
    /// Resurrect a document the harness believes is deleted.
    Resurrect { id: String, value: i64 },
    /// A bulk write mixing rows that must succeed with rows that must conflict.
    MixedBulk { rows: Vec<MixedRow> },
    /// Read everything back and check the invariants that need no new state.
    CheckOnly,
}

/// One row of a mixed bulk write, with the outcome the harness predicts.
#[derive(Clone, Debug)]
pub struct MixedRow {
    /// The document to write.
    pub id: String,
    /// New value.
    pub value: i64,
    /// What the row is trying to do.
    pub intent: RowIntent,
}

/// What a row is trying to do, which determines whether it must succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowIntent {
    /// No `previous` against an absent document: must succeed.
    FreshInsert,
    /// No `previous` against an existing document: must be a 409.
    ConflictInsert,
    /// `previous` matches the stored revision: must succeed.
    CorrectUpdate,
    /// `previous` is one revision stale: must be a 409.
    StaleUpdate,
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// What the harness believes is stored, derived only from operations it believes succeeded.
///
/// This is the "no lost writes" oracle: after every operation it is compared against what the
/// storage actually returns. It is deliberately a *model of the harness's own claims* — it
/// never asks the storage what happened — so a storage that silently drops a write diverges
/// from it rather than moving it.
#[derive(Clone, Debug, Default)]
struct Model {
    /// Document id -> (value, deleted, revision).
    docs: BTreeMap<String, ModelDoc>,
}

#[derive(Clone, Debug)]
struct ModelDoc {
    value: i64,
    deleted: bool,
    revision: String,
}

impl Model {
    fn live_ids(&self) -> BTreeSet<&String> {
        self.docs
            .iter()
            .filter(|(_, doc)| !doc.deleted)
            .map(|(id, _)| id)
            .collect()
    }

    fn all_ids(&self) -> BTreeSet<&String> {
        self.docs.keys().collect()
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// Generates a random operation sequence from `rng`.
///
/// The generator is intentionally "uninformed": it picks ids from a small pool so collisions
/// are common, and picks operation kinds uniformly. A model-aware generator would produce
/// tidier sequences and find fewer bugs, because real bugs live in the cases a tidier
/// generator filters out.
fn generate_ops(rng: &util::Rng, count: usize, id_pool: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        let id = format!("id{}", draw(rng, id_pool));
        let value = draw(rng, 1000) as i64;
        let op = match draw(rng, 9) {
            0 => Op::Insert { id, value },
            1 => Op::InsertOverExisting { id, value },
            2 => Op::Update { id, value },
            3 => Op::StaleUpdate { id, value },
            4 => Op::Delete { id },
            5 => Op::DeleteTwice { id },
            6 => Op::Resurrect { id, value },
            7 => {
                let rows = (0..(1 + draw(rng, 3)))
                    .map(|_| MixedRow {
                        id: format!("id{}", draw(rng, id_pool)),
                        value: draw(rng, 1000) as i64,
                        intent: match draw(rng, 4) {
                            0 => RowIntent::FreshInsert,
                            1 => RowIntent::ConflictInsert,
                            2 => RowIntent::CorrectUpdate,
                            _ => RowIntent::StaleUpdate,
                        },
                    })
                    .collect();
                Op::MixedBulk { rows }
            }
            _ => Op::CheckOnly,
        };
        ops.push(op);
    }
    ops
}

/// [`generate_ops`] for the integration test, which lives in another file.
///
/// The generator is public so the differential test can generate one sequence and hand the
/// *same* sequence to every storage. Generating per storage from the same seed would be
/// equivalent only if the generator were pure, and making that guarantee explicit at the call
/// site is cheaper than reasoning about it.
pub fn generate_ops_public(rng: &util::Rng, count: usize, id_pool: usize) -> Vec<Op> {
    generate_ops(rng, count, id_pool)
}

/// Draws a value in `0..bound` from `rng`, independent of the crate's global generator.
///
/// Sequenced here rather than through [`dxdb_testkit::util`]'s global so that a seed fully
/// determines a run: two storages given one seed must generate byte-identical sequences, and
/// a global generator shared with the storage's own internals would break that.
fn draw(rng: &util::Rng, bound: usize) -> usize {
    debug_assert!(bound > 0);
    ((rng.next_f64() * bound as f64) as usize).min(bound - 1)
}

/// Everything one fuzz run learned, for a differential comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// The seed.
    pub seed: u64,
    /// The storage's reported name.
    pub storage: String,
    /// Final live ids, sorted.
    pub live_ids: Vec<String>,
    /// Final total document count including tombstones.
    pub all_ids: Vec<String>,
    /// Final id -> value for live documents.
    pub values: BTreeMap<String, i64>,
    /// Final id -> `_rev` for every document.
    pub revisions: BTreeMap<String, String>,
    /// The number of operations that were expected to fail and did.
    pub expected_conflicts: usize,
}

/// Runs one generated sequence against a storage.
///
/// # Panics
///
/// Panics on any invariant violation, with the seed, the operation index and the operation
/// log so the failure is reproducible and readable. A harness that reports "invariant failed"
/// without the sequence is a harness somebody has to re-derive the bug from.
pub async fn run_sequence<S: RxStorage>(
    storage: &S,
    seed: u64,
    ops: &[Op],
    log: &mut Vec<String>,
) -> Result<RunOutcome, String> {
    let spec = InstanceSpec::new(fill_with_default_settings(
        dxdb_testkit::schemas::pseudo_schema_for_version(0, "key"),
    ));
    let primary = spec.storage_schema().primary_path.clone();
    let instance = storage
        .create_storage_instance(spec.params())
        .await
        .map_err(|err| format!("seed {seed}: create_storage_instance failed: {err}"))?;

    let mut model = Model::default();
    let mut revisions: BTreeMap<String, String> = BTreeMap::new();
    let mut next_height: BTreeMap<String, u32> = BTreeMap::new();
    let mut expected_conflicts = 0usize;

    for (index, op) in ops.iter().enumerate() {
        log.push(format!("{index:03} {op:?}"));
        let step = apply(storage, &instance, &primary, op, &mut model, &mut revisions, &mut next_height)
            .await;
        if let Err(err) = step {
            return Err(format!(
                "seed {seed}, operation {index} ({op:?})\n  {err}\n\n  operation log:\n{}",
                log.join("\n")
            ));
        }
        if matches!(
            op,
            Op::InsertOverExisting { .. }
                | Op::StaleUpdate { .. }
                | Op::MixedBulk { .. }
        ) {
            expected_conflicts += 1;
        }

        // Invariants that must hold after *every* operation, not just at the end.
        check_invariants(storage, &instance, &primary, &model, &mut revisions, seed, index, &log)
            .await?;
    }

    let final_docs = all_documents(&instance, &primary).await?;
    Ok(RunOutcome {
        seed,
        storage: storage.name().to_string(),
        live_ids: final_docs
            .iter()
            .filter(|doc| !doc.is_deleted())
            .map(|doc| id_of(doc, &primary))
            .collect::<Result<Vec<_>, _>>()?,
        all_ids: final_docs
            .iter()
            .map(|doc| id_of(doc, &primary))
            .collect::<Result<Vec<_>, _>>()?,
        values: final_docs
            .iter()
            .filter(|doc| !doc.is_deleted())
            .map(|doc| {
                let id = id_of(doc, &primary)?;
                let value = doc
                    .get("value")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| format!("document {id} has no integer value"))?;
                Ok((id, value))
            })
            .collect::<Result<_, String>>()?,
        revisions: revisions.clone(),
        expected_conflicts,
    })
}

/// Applies one operation, updating the model, and asserts the *predicted* outcome.
async fn apply<S: RxStorage>(
    _storage: &S,
    instance: &Box<DynRxStorageInstance<'static>>,
    primary: &str,
    op: &Op,
    model: &mut Model,
    revisions: &mut BTreeMap<String, String>,
    next_height: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    match op {
        Op::Insert { id, value } => {
            if model.docs.contains_key(id) {
                // Generating an insert on an existing id is fine; it must conflict.
                let response = instance
                    .bulk_write(vec![insert_row(id, *value, 1)], "fuzz".into())
                    .await
                    .map_err(|e| format!("bulk_write failed outright: {e}"))?;
                assert_conflict(&response.error, id, "insert over an existing document")?;
                return Ok(());
            }
            write_document(instance, primary, id, *value, false, None, model, revisions, next_height).await
        }
        Op::InsertOverExisting { id, value } => {
            let response = instance
                .bulk_write(vec![insert_row(id, *value, 1)], "fuzz".into())
                .await
                .map_err(|e| format!("bulk_write failed outright: {e}"))?;
            if model.docs.contains_key(id) {
                assert_conflict(&response.error, id, "insert over an existing document")?;
            } else {
                assert_ok(&response.error, id, "insert of a fresh document")?;
                let revision = staged_revision(id, next_height);
                model.docs.insert(
                    id.clone(),
                    ModelDoc {
                        value: *value,
                        deleted: false,
                        revision: revision.clone(),
                    },
                );
                revisions.insert(id.clone(), revision);
            }
            Ok(())
        }
        Op::Update { id, value } => {
            let Some(existing) = model.docs.get(id).cloned() else {
                // Nothing to update; the generator does not know the state. Skip rather
                // than assert, because "update a document that was never written" is a
                // different case (and is covered by StaleUpdate).
                return Ok(());
            };
            let previous = fetch(instance, id).await?;
            let Some(previous) = previous else {
                return Err(format!("model believes {id} exists but the storage does not"));
            };
            write_document(
                instance,
                primary,
                id,
                *value,
                existing.deleted,
                Some(previous),
                model,
                revisions,
                next_height,
            )
            .await
        }
        Op::StaleUpdate { id, value } => {
            if !model.docs.contains_key(id) {
                return Ok(());
            }
            // A `previous` with revision height 1 when the stored height is higher is stale.
            // When the stored height is already 1 this is not stale at all, so the row is
            // written from the current state instead — predicting the outcome rather than
            // hoping is the point of having a model.
            let current = fetch(instance, id).await?;
            let Some(current) = current else {
                return Err(format!("model believes {id} exists but the storage does not"));
            };
            let height = dxdb_storage::get_height_of_revision(current.rev().unwrap_or("1-x"));
            if height > 1 {
                let stale = stale_previous(id, *value, current.rev().unwrap_or("2-x"));
                let response = instance
                    .bulk_write(vec![stale], "fuzz".into())
                    .await
                    .map_err(|e| format!("bulk_write failed outright: {e}"))?;
                assert_conflict(&response.error, id, "update from a stale previous")?;
                Ok(())
            } else {
                let deleted = model.docs.get(id).map(|doc| doc.deleted).unwrap_or(false);
                write_document(
                    instance,
                    primary,
                    id,
                    *value,
                    deleted,
                    Some(current),
                    model,
                    revisions,
                    next_height,
                )
                .await
            }
        }
        Op::Delete { id } => {
            let Some(existing) = model.docs.get(id).cloned() else {
                return Ok(());
            };
            let previous = fetch(instance, id).await?;
            let Some(previous) = previous else {
                return Err(format!("model believes {id} exists but the storage does not"));
            };
            write_document(
                instance,
                primary,
                id,
                existing.value,
                true,
                Some(previous),
                model,
                revisions,
                next_height,
            )
            .await
        }
        Op::DeleteTwice { id } => {
            let Some(existing) = model.docs.get(id).cloned() else {
                return Ok(());
            };
            if !existing.deleted {
                return Ok(());
            }
            // A tombstone is a document: writing it again must be accepted, which is the
            // "should be able to update the state of a deleted document" contract.
            let previous = fetch(instance, id).await?;
            let Some(previous) = previous else {
                return Err(format!("model believes tombstone {id} exists but the storage does not"));
            };
            write_document(
                instance,
                primary,
                id,
                existing.value,
                true,
                Some(previous),
                model,
                revisions,
                next_height,
            )
            .await
        }
        Op::Resurrect { id, value } => {
            let Some(existing) = model.docs.get(id).cloned() else {
                return Ok(());
            };
            if !existing.deleted {
                return Ok(());
            }
            let previous = fetch(instance, id).await?;
            let Some(previous) = previous else {
                return Err(format!("model believes {id} exists but the storage does not"));
            };
            write_document(
                instance,
                primary,
                id,
                *value,
                false,
                Some(previous),
                model,
                revisions,
                next_height,
            )
            .await
        }
        Op::MixedBulk { rows } => {
            // Per-document atomicity: the rows that must succeed must succeed, and the rows
            // that must conflict must conflict — in the *same* call. This is the invariant
            // PLAN.md 4.2 calls load-bearing, and the one a storage gets wrong by wrapping
            // the whole bulk in a transaction.
            // Expectations are tracked **by row index**, not by document id.
            //
            // A bulk may legitimately contain two rows for one id (a stale update and a
            // fresh insert, say), and an earlier version keyed its expectations by id — so a
            // row that was skipped got credited with the other row's success, the model then
            // recorded a revision that never happened, and the next invariant check reported
            // a monotonicity violation against a correct storage. Index-keyed is the only
            // correct bookkeeping for a list of rows.
            let mut write_rows: Vec<BulkWriteRow> = Vec::new();
            let mut outcomes: Vec<Option<bool>> = Vec::new(); // Some(true)=ok, Some(false)=409

            for row in rows {
                let push = |write_rows: &mut Vec<BulkWriteRow>,
                            outcomes: &mut Vec<Option<bool>>,
                            row: BulkWriteRow,
                            ok: bool| {
                    write_rows.push(row);
                    outcomes.push(Some(ok));
                };
                match row.intent {
                    RowIntent::FreshInsert => {
                        // An insert with no `previous` over an existing document is a 409,
                        // so the model decides the prediction rather than the generator's
                        // guess about the state.
                        let ok = !model.docs.contains_key(&row.id);
                        push(&mut write_rows, &mut outcomes, insert_row(&row.id, row.value, 1), ok);
                    }
                    RowIntent::ConflictInsert => {
                        if !model.docs.contains_key(&row.id) {
                            // Nothing to conflict with; skip the row rather than guess.
                            continue;
                        }
                        push(&mut write_rows, &mut outcomes, insert_row(&row.id, row.value, 1), false);
                    }
                    RowIntent::CorrectUpdate => {
                        let Some(existing) = model.docs.get(&row.id).cloned() else {
                            continue;
                        };
                        let Some(previous) = fetch(instance, &row.id).await? else {
                            continue;
                        };
                        push(
                            &mut write_rows,
                            &mut outcomes,
                            update_row_from(previous, &row.id, row.value, existing.deleted, next_height),
                            true,
                        );
                    }
                    RowIntent::StaleUpdate => {
                        if !model.docs.contains_key(&row.id) {
                            continue;
                        }
                        let Some(current) = fetch(instance, &row.id).await? else {
                            continue;
                        };
                        let height =
                            dxdb_storage::get_height_of_revision(current.rev().unwrap_or("1-x"));
                        if height <= 1 {
                            // Not actually stale; skip rather than mispredict.
                            continue;
                        }
                        push(
                            &mut write_rows,
                            &mut outcomes,
                            stale_previous(&row.id, row.value, current.rev().unwrap_or("2-x")),
                            false,
                        );
                    }
                }
            }

            // A bulk write containing **two rows for the same document** is a genuinely
            // ambiguous case: whichever row the storage applies first determines whether the
            // second sees a matching `previous`, so "row 2 must succeed" is not a prediction
            // any model can make without knowing the storage's intra-bulk ordering — which
            // the trait does not specify. Rather than assert something the contract does not
            // promise, drop the duplicates and keep the first row per id.
            //
            // This is a real limit of the harness, recorded rather than papered over: the
            // interaction between two rows for one id inside one bulk is *not* covered here.
            // It is worth a hand-written case once the contract says what should happen.
            {
                let mut seen: BTreeSet<String> = BTreeSet::new();
                let mut kept_rows = Vec::new();
                let mut kept_outcomes = Vec::new();
                for (row, outcome) in write_rows.into_iter().zip(outcomes.into_iter()) {
                    let id = row
                        .document
                        .id("key")
                        .expect("fuzz rows always carry a key")
                        .to_string();
                    if seen.insert(id) {
                        kept_rows.push(row);
                        kept_outcomes.push(outcome);
                    }
                }
                write_rows = kept_rows;
                outcomes = kept_outcomes;
            }

            if write_rows.is_empty() {
                return Ok(());
            }

            // Record the predicted outcomes alongside the rows so the model update below is
            // keyed the same way the assertion was.
            let predictions: Vec<(String, i64, bool)> = write_rows
                .iter()
                .zip(outcomes.iter())
                .map(|(row, ok)| {
                    let id = row
                        .document
                        .id("key")
                        .expect("fuzz rows always carry a key")
                        .to_string();
                    let value = row
                        .document
                        .document()
                        .get("value")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    (id, value, ok.expect("every pushed row has a prediction"))
                })
                .collect();

            let response = instance
                .bulk_write(write_rows, "fuzz".into())
                .await
                .map_err(|e| format!("bulk_write failed outright: {e}"))?;

            // Per-document atomicity: within *one* call, the rows predicted to succeed must
            // succeed and the rows predicted to conflict must conflict.
            for (id, _, predicted_ok) in &predictions {
                if *predicted_ok {
                    assert_ok(&response.error, id, "row predicted to succeed")?;
                } else {
                    assert_conflict(&response.error, id, "row predicted to conflict")?;
                }
            }

            // Update the model only for rows that succeeded.
            for (id, value, predicted_ok) in predictions {
                if !predicted_ok {
                    continue;
                }
                let previous_revision = model
                    .docs
                    .get(&id)
                    .map(|doc| doc.revision.clone())
                    .unwrap_or_default();
                let revision = next_revision(&previous_revision, next_height, &id);
                let deleted = model.docs.get(&id).map(|doc| doc.deleted).unwrap_or(false);
                revisions.insert(id.clone(), revision.clone());
                model.docs.insert(
                    id,
                    ModelDoc {
                        value,
                        deleted,
                        revision,
                    },
                );
            }
            Ok(())
        }
        Op::CheckOnly => Ok(()),
    }
}

/// Writes one document from `previous` (or as an insert) and records the model.
///
/// The model update is not optional and not a formality: it is the "no lost writes" oracle.
/// An earlier version updated only `revisions` and not `model.docs`, which meant every
/// `Update`, `Delete`, `DeleteTwice` and `Resurrect` left the model describing the state
/// before the write — and the very next invariant check then reported the storage as wrong
/// while the storage was right. A harness that mispredicts its own model is worse than no
/// harness, because every failure it reports has to be disproved first.
#[allow(clippy::too_many_arguments)]
async fn write_document(
    instance: &Box<DynRxStorageInstance<'static>>,
    primary: &str,
    id: &str,
    value: i64,
    deleted: bool,
    previous: Option<DocumentData>,
    model: &mut Model,
    revisions: &mut BTreeMap<String, String>,
    next_height: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    let _ = primary;
    let previous_revision = previous
        .as_ref()
        .and_then(|doc| doc.rev())
        .unwrap_or("")
        .to_string();
    let revision = next_revision(&previous_revision, next_height, id);
    let document = build_document(id, value, deleted, &revision);
    let row = match previous {
        Some(previous) => BulkWriteRow::update(previous, DocumentWriteData::new(document)),
        None => BulkWriteRow::insert(DocumentWriteData::new(document)),
    };
    let response = instance
        .bulk_write(vec![row], "fuzz".into())
        .await
        .map_err(|e| format!("bulk_write failed outright: {e}"))?;
    assert_ok(&response.error, id, "single-row write")?;
    revisions.insert(id.to_string(), revision.clone());
    model.docs.insert(
        id.to_string(),
        ModelDoc {
            value,
            deleted,
            revision,
        },
    );
    Ok(())
}

/// The next revision string for `id`, advancing the height.
///
/// Height comes from the *previous* revision, so a resurrect or a delete-twice always moves
/// forward. Reusing a height is the monotonicity bug this catches.
fn next_revision(previous: &str, next_height: &mut BTreeMap<String, u32>, id: &str) -> String {
    let height = if previous.is_empty() {
        1
    } else {
        dxdb_storage::get_height_of_revision(previous) + 1
    };
    let entry = next_height.entry(id.to_string()).or_insert(0);
    *entry = (*entry).max(height);
    format!("{}-fuzzfuzzfuzzfuzzfuzzfuzzfuzzfu", height)
}

/// A revision for a document whose height is not tracked (used where the model is staged).
fn staged_revision(id: &str, next_height: &mut BTreeMap<String, u32>) -> String {
    let entry = next_height.entry(id.to_string()).or_insert(1);
    format!("{}-fuzzfuzzfuzzfuzzfuzzfuzzfuzzfu", *entry)
}

/// Builds a document JSON with the four meta fields.
fn build_document(id: &str, value: i64, deleted: bool, revision: &str) -> DocumentData {
    let mut map = Map::new();
    map.insert("key".to_string(), Value::String(id.to_string()));
    map.insert("value".to_string(), json!(value));
    map.insert("_deleted".to_string(), Value::Bool(deleted));
    map.insert("_attachments".to_string(), json!({}));
    // A fixed, past `lwt`: the fuzz harness is not testing retention, and a moving clock
    // would make the "no lost writes" comparison non-deterministic.
    map.insert("_meta".to_string(), json!({ "lwt": 1_700_000_000_000.0 }));
    map.insert("_rev".to_string(), Value::String(revision.to_string()));
    DocumentData::from_value(Value::Object(map)).expect("fuzz document is an object")
}

fn insert_row(id: &str, value: i64, height: u32) -> BulkWriteRow {
    let revision = format!("{height}-fuzzfuzzfuzzfuzzfuzzfuzzfuzzfu");
    BulkWriteRow::insert(DocumentWriteData::new(build_document(
        id, value, false, &revision,
    )))
}

fn update_row_from(
    previous: DocumentData,
    id: &str,
    value: i64,
    deleted: bool,
    next_height: &mut BTreeMap<String, u32>,
) -> BulkWriteRow {
    let previous_revision = previous.rev().unwrap_or("").to_string();
    let revision = next_revision(&previous_revision, next_height, id);
    BulkWriteRow::update(
        previous,
        DocumentWriteData::new(build_document(id, value, deleted, &revision)),
    )
}

/// A row whose `previous` claims a revision the document does not have.
fn stale_previous(id: &str, value: i64, current_revision: &str) -> BulkWriteRow {
    // Height 1 when the stored height is higher is guaranteed stale, and the document id is
    // real so the conflict is about the revision rather than about a missing document.
    let stale_revision = format!("{}-fuzzfuzzfuzzfuzzfuzzfuzzfuzzfu", 1);
    debug_assert_ne!(stale_revision, current_revision);
    let stale_doc = build_document(id, value, false, &stale_revision);
    BulkWriteRow::update(
        stale_doc,
        DocumentWriteData::new(build_document(id, value + 1, false, "2-fuzzfuzzfuzzfuzzfuzzfuzzfuzzfu")),
    )
}

fn assert_ok(errors: &[WriteError], id: &str, what: &str) -> Result<(), String> {
    if errors.is_empty() {
        return Ok(());
    }
    let mine: Vec<String> = errors
        .iter()
        .filter(|error| error.document_id() == id)
        .map(|error| format!("{}: {:?}", error.status(), error.document_id()))
        .collect();
    if mine.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{what} for {id} was predicted to succeed but produced errors: {mine:?}"
    ))
}

fn assert_conflict(errors: &[WriteError], id: &str, what: &str) -> Result<(), String> {
    let found = errors
        .iter()
        .find(|error| error.document_id() == id && error.status() == 409);
    match found {
        Some(_) => Ok(()),
        None => {
            let all: Vec<String> = errors
                .iter()
                .map(|error| format!("{}:{}", error.status(), error.document_id()))
                .collect();
            Err(format!(
                "{what} for {id} was predicted to conflict (409) but produced: {all:?}"
            ))
        }
    }
}

async fn fetch(
    instance: &Box<DynRxStorageInstance<'static>>,
    id: &str,
) -> Result<Option<DocumentData>, String> {
    let docs = instance
        .find_documents_by_id(vec![id.to_string()], true)
        .await
        .map_err(|e| format!("find_documents_by_id failed: {e}"))?;
    Ok(docs.into_iter().next())
}

/// Every document in the collection, live and deleted, via `query`.
async fn all_documents(
    instance: &Box<DynRxStorageInstance<'static>>,
    primary: &str,
) -> Result<Vec<DocumentData>, String> {
    let prepared = dxdb_testkit::harness::build_prepared_query_for_schema(
        &json!({ "selector": {}, "sort": [{ primary: "asc" }] }),
        instance.schema(),
    );
    instance
        .query(prepared)
        .await
        .map_err(|e| format!("query failed: {e}"))
}

fn id_of(doc: &DocumentData, primary: &str) -> Result<String, String> {
    doc.id(primary)
        .map(str::to_string)
        .map_err(|e| format!("document has no primary value: {e}"))
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

/// Checks every invariant that can be evaluated from the current state.
///
/// Run after *every* operation, not just at the end: a lost write that a later operation
/// happens to overwrite would be invisible to a final-state check, and the whole point is to
/// catch the operation that broke it.
#[allow(clippy::too_many_arguments)]
async fn check_invariants<S: RxStorage>(
    _storage: &S,
    instance: &Box<DynRxStorageInstance<'static>>,
    primary: &str,
    model: &Model,
    revisions: &mut BTreeMap<String, String>,
    seed: u64,
    index: usize,
    log: &[String],
) -> Result<(), String> {
    let context = || {
        format!(
            "seed {seed}, after operation {index}\n\n  operation log:\n{}",
            log.join("\n")
        )
    };

    let all = all_documents(instance, primary).await?;

    // 1. No lost writes: every id the model believes exists is readable, and ids the model
    //    does not know about do not appear.
    let stored_all: BTreeSet<String> = all.iter().map(|d| id_of(d, primary)).collect::<Result<_, _>>()?;
    let model_all: BTreeSet<String> = model.all_ids().into_iter().cloned().collect();
    if stored_all != model_all {
        return Err(format!(
            "{}: the set of stored documents disagrees with the model\n  \
             only in storage: {:?}\n  only in model:   {:?}",
            context(),
            stored_all.difference(&model_all).collect::<Vec<_>>(),
            model_all.difference(&stored_all).collect::<Vec<_>>()
        ));
    }

    // 2. count == query().len(), for a selector that includes tombstones and one that does
    //    not. The two paths are independent in most storages, which is the point.
    for selector in [json!({}), json!({"_deleted": false}), json!({"_deleted": true})] {
        let prepared = dxdb_testkit::harness::build_prepared_query_for_schema(
            &json!({ "selector": selector, "sort": [{ primary: "asc" }] }),
            instance.schema(),
        );
        let queried = instance
            .query(prepared.clone())
            .await
            .map_err(|e| format!("{}: query(selector={selector}) failed: {e}", context()))?;
        let counted = instance
            .count(prepared)
            .await
            .map_err(|e| format!("{}: count(selector={selector}) failed: {e}", context()))?;
        if counted.count != queried.len() {
            return Err(format!(
                "{}: count() != query().len() for selector {selector}\n  \
                 count = {}, query = {}",
                context(),
                counted.count,
                queried.len()
            ));
        }
    }

    // 3. find_documents_by_id agrees with the scan, for every id the model knows.
    let ids: Vec<String> = model.all_ids().into_iter().cloned().collect();
    if !ids.is_empty() {
        let by_id = instance
            .find_documents_by_id(ids.clone(), true)
            .await
            .map_err(|e| format!("{}: find_documents_by_id failed: {e}", context()))?;
        let by_id_ids: BTreeSet<String> =
            by_id.iter().map(|d| id_of(d, primary)).collect::<Result<_, _>>()?;
        let from_scan: BTreeSet<String> = stored_all.clone();
        if by_id_ids != from_scan {
            return Err(format!(
                "{}: find_documents_by_id disagrees with query()\n  \
                 only by id: {:?}\n  only by scan: {:?}",
                context(),
                by_id_ids.difference(&from_scan).collect::<Vec<_>>(),
                from_scan.difference(&by_id_ids).collect::<Vec<_>>()
            ));
        }

        // 4. Monotonic revisions: a stored `_rev` never moves backwards, and the value the
        //    model expects is the value stored.
        for doc in &all {
            let id = id_of(doc, primary)?;
            let Some(stored) = doc.rev() else {
                return Err(format!("{}: document {id} has no _rev", context()));
            };
            let height = dxdb_storage::get_height_of_revision(stored);
            if let Some(previous) = revisions.get(&id) {
                let previous_height = dxdb_storage::get_height_of_revision(previous);
                if height < previous_height {
                    return Err(format!(
                        "{}: revision of {id} went backwards: {} -> {}",
                        context(),
                        previous,
                        stored
                    ));
                }
            }
            revisions.insert(id, stored.to_string());
        }
    }

    // 5. Live documents carry the model's value; tombstones are invisible to a live-only
    //    selector. (4) already pinned the revision; this pins the payload, which is what a
    //    merge-instead-of-replace bug corrupts.
    for doc in &all {
        let id = id_of(doc, primary)?;
        let Some(expected) = model.docs.get(&id) else {
            continue;
        };
        if doc.is_deleted() != expected.deleted {
            return Err(format!(
                "{}: document {id} deleted flag is {} but the model says {}",
                context(),
                doc.is_deleted(),
                expected.deleted
            ));
        }
        if !expected.deleted {
            let value = doc.get("value").and_then(Value::as_i64);
            if value != Some(expected.value) {
                return Err(format!(
                    "{}: live document {id} has value {value:?} but the model says {}",
                    context(),
                    expected.value
                ));
            }
        }
    }

    Ok(())
}

/// The seeds a default run uses.
///
/// Deterministic and fixed, so CI is reproducible; `DXDB_FUZZ_SEEDS` and `DXDB_FUZZ_SEED`
/// override for a longer local run or a single replay.
pub fn seeds() -> Vec<u64> {
    if let Ok(single) = std::env::var("DXDB_FUZZ_SEED") {
        if let Ok(seed) = single.trim().parse::<u64>() {
            return vec![seed];
        }
    }
    let count = std::env::var("DXDB_FUZZ_SEEDS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(24);
    // A fixed base rather than `0..count`, so seed N means the same thing across runs and
    // across storages.
    (0..count as u64).map(|n| 0x5EED_F022 + n).collect()
}

/// The operation count per sequence.
pub fn ops_per_sequence() -> usize {
    std::env::var("DXDB_FUZZ_OPS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(60)
}

/// The id pool size, i.e. how often operations collide.
pub fn id_pool() -> usize {
    std::env::var("DXDB_FUZZ_IDS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(6)
}
