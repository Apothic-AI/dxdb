//! `replication-transcripts.json` — replicated state and checkpoints over time.
//!
//! A replication is not a pure function, so it is pinned by transcripts: a scripted
//! sequence of writes and syncs, and after each step the observable state of the fork
//! (client), the master, and the meta instance.
//!
//! Reading notes, all of which are things a hand-rolled reader gets wrong:
//!
//! - **`meta` is not "checkpoints".** It holds one document per tracked item
//!   (`<itemId>|0`, the fork's assumption about the master) *and* one per direction
//!   (`down|1`, `up|1`, the actual checkpoints). Filter on
//!   [`MetaDocument::is_checkpoint`] before treating an entry as a checkpoint.
//! - **Snapshots deliberately omit `_meta.lwt` and `_rev` on meta documents.** Upstream
//!   stamps those with `now()` and derives the revision from a checkpoint key, and the
//!   409-retry path consumes a variable number of clock ticks depending on promise
//!   scheduling. They are write bookkeeping, not protocol behaviour, so they are not
//!   recorded. The header documents this under
//!   [`Determinism::excluded_from_meta_snapshots`]. Document snapshots (`master`,
//!   `fork`) *do* carry `_meta`, because that value the harness controls.
//! - **`fork` documents can carry `_meta.o`.** That is upstream's replication origin
//!   tag (`RxDocumentMeta.o`, a revision number and hash), present when a document
//!   arrived from a replication rather than being written locally. It is kept.
//! - **`case_count` counts snapshots, not transcripts.** 13 snapshots across 6
//!   transcripts, plus the batch-boundary entry.

use serde::Deserialize;

use crate::FamilyFile;

/// The decoded `replication-transcripts.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vectors {
    /// The documentation comment the generator wrote into the file.
    #[serde(rename = "$comment")]
    pub comment: String,
    /// The generator that wrote the file, e.g. `xtask/vectors/generate.mjs`.
    pub generator: String,
    /// Informational Node version; `--check` ignores it, so it may differ locally.
    pub node: String,
    /// Upstream RxDB version the vectors came from. Absent for `jsnum`, which only
    /// needs Node.
    #[serde(default, rename = "rxdbVersion")]
    pub rxdb_version: Option<String>,
    /// Upstream RxDB commit the vectors came from. Same condition as `rxdb_version`.
    #[serde(default, rename = "rxdbCommit")]
    pub rxdb_commit: Option<String>,
    /// The recorded product.
    pub format: String,
    /// The determinism stubs in force, and what they make unrecordable.
    pub determinism: Determinism,
    /// Number of transcripts in [`Vectors::transcripts`].
    #[serde(rename = "transactionCount")]
    pub transaction_count: usize,
    /// Number of recorded steps, across all transcripts and the batch boundary.
    ///
    /// This is the figure the case-count guard checks, because it is what a consumer
    /// iterates over.
    #[serde(rename = "caseCount")]
    pub case_count: usize,
    /// The transcripts.
    pub transcripts: Vec<Transcript>,
    /// The pull-batch-size boundary case, which lives outside the transcripts because
    /// it needs its own replication instance with a small batch size.
    #[serde(rename = "batchBoundary")]
    pub batch_boundary: Vec<BatchBoundary>,
}

impl FamilyFile for Vectors {
    fn provenance(&self) -> crate::Provenance {
        crate::Provenance {
            comment: self.comment.clone(),
            generator: self.generator.clone(),
            node: self.node.clone(),
            rxdb_version: self.rxdb_version.clone(),
            rxdb_commit: self.rxdb_commit.clone(),
        }
    }
}

impl Vectors {
    /// The provenance header of this file.
    ///
    /// Inherent rather than trait-only so a consumer can call it without importing
    /// [`FamilyFile`]; the trait exists for the loader's generic bound.
    pub fn provenance(&self) -> crate::Provenance {
        FamilyFile::provenance(self)
    }
}

impl Vectors {
    /// Every meta snapshot in file order: each transcript's steps, then the batch
    /// boundary entry.
    ///
    /// The batch-boundary entry is not a [`Step`] — it has no `step` label and no
    /// master/fork arrays, because it only records what the fork ended up with. It is
    /// therefore normalized to just the part that is comparable: its meta documents.
    pub fn all_meta_snapshots(&self) -> impl Iterator<Item = &[MetaDocument]> {
        self.transcripts
            .iter()
            .flat_map(|t| t.steps.iter().map(|s| s.meta.as_slice()))
            .chain(self.batch_boundary.iter().map(|b| b.snapshot.0.as_slice()))
    }

    /// The snapshot count, matching [`Vectors::case_count`] for this family.
    pub fn case_count(&self) -> usize {
        self.transcripts.iter().map(|t| t.steps.len()).sum::<usize>() + self.batch_boundary.len()
    }

    /// Look up a transcript by its label, e.g. `deletion-propagates`.
    pub fn transcript(&self, why: &str) -> Option<&Transcript> {
        self.transcripts.iter().find(|t| t.why == why)
    }
}

/// The determinism stubs, and what they make unrepresentable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    /// How `Math.random` was replaced.
    #[serde(rename = "mathRandom")]
    pub math_random: String,
    /// How `Date.now` was replaced.
    #[serde(rename = "dateNow")]
    pub date_now: String,
    /// A longer note about why document `_meta.lwt` values are reproducible.
    pub note: String,
    /// Fields the generator deliberately omits from meta snapshots, and why.
    #[serde(rename = "excludedFromMetaSnapshots")]
    pub excluded_from_meta_snapshots: ExcludedFields,
}

/// Fields deliberately not recorded in meta snapshots.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludedFields {
    /// The field names that were omitted.
    pub fields: Vec<String>,
    /// The reasoning. Read this before "restoring" the fields in a generator change.
    pub why: String,
}

/// One scripted replication, step by step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    /// The scenario label.
    pub why: String,
    /// Snapshots taken during the script, in order.
    pub steps: Vec<Step>,
    /// How many times the conflict handler ran during the whole transcript.
    #[serde(rename = "recordedConflictResolutions")]
    pub recorded_conflict_resolutions: usize,
    /// Errors emitted by the replication. Expected empty.
    pub errors: Vec<String>,
    /// The final fork snapshot, repeated for convenience.
    #[serde(rename = "finalForkState")]
    pub final_fork_state: Vec<DocumentSnapshot>,
    /// The final master snapshot, repeated for convenience.
    #[serde(rename = "finalMasterState")]
    pub final_master_state: Vec<DocumentSnapshot>,
    /// The final meta snapshot, repeated for convenience.
    #[serde(rename = "finalMetaState")]
    pub final_meta_state: Vec<MetaDocument>,
}

impl Transcript {
    /// The last recorded step, if any.
    pub fn last_step(&self) -> Option<&Step> {
        self.steps.last()
    }

    /// Checkpoint documents only, in the final snapshot.
    pub fn final_checkpoints(&self) -> impl Iterator<Item = &MetaDocument> {
        self.final_meta_state.iter().filter(|m| m.is_checkpoint())
    }
}

/// One snapshot of all three instances.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The step label, e.g. `after-pull`.
    pub step: String,
    /// The master's documents, ordered by primary key.
    pub master: Vec<DocumentSnapshot>,
    /// The fork's documents, ordered by primary key.
    pub fork: Vec<DocumentSnapshot>,
    /// The meta instance's documents, ordered by id.
    pub meta: Vec<MetaDocument>,
}

impl Step {
    /// Look up a master document by its `passportId`.
    pub fn master_doc(&self, id: &str) -> Option<&DocumentSnapshot> {
        self.master.iter().find(|d| d.passport_id == id)
    }

    /// Look up a fork document by its `passportId`.
    pub fn fork_doc(&self, id: &str) -> Option<&DocumentSnapshot> {
        self.fork.iter().find(|d| d.passport_id == id)
    }

    /// The `down|1` checkpoint, i.e. how far the fork has pulled.
    pub fn down_checkpoint(&self) -> Option<&MetaDocument> {
        self.meta.iter().find(|m| m.item_id == "down")
    }

    /// The `up|1` checkpoint, i.e. how far the fork has pushed.
    pub fn up_checkpoint(&self) -> Option<&MetaDocument> {
        self.meta.iter().find(|m| m.item_id == "up")
    }
}

/// A document as it appears in a `master` or `fork` snapshot.
///
/// Only the fields the generator records are present; the family is about replication
/// state, not about carrying whole documents.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSnapshot {
    /// The primary key value.
    #[serde(rename = "passportId")]
    pub passport_id: String,
    /// The full revision, including its `height-hash` structure.
    #[serde(rename = "_rev")]
    pub rev: String,
    /// Whether this is a tombstone.
    #[serde(rename = "_deleted")]
    pub deleted: bool,
    /// The document meta, including the replication origin tag when present.
    #[serde(rename = "_meta")]
    pub meta: DocumentMeta,
    /// The fixture's numeric field, used by the age-based conflict handler.
    pub age: i64,
    /// The fixture's string field.
    #[serde(rename = "firstName")]
    pub first_name: String,
}

/// `_meta` on a snapshot document.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentMeta {
    /// Last write time, in Unix milliseconds with two decimals.
    pub lwt: f64,
    /// The replication origin tag, present when the document arrived by replication.
    ///
    /// Mirrors upstream's `RxDocumentMeta.o`: a revision number and the hash half of
    /// the revision, so a later replication can tell whether a write was its own.
    #[serde(default)]
    pub o: Option<ReplicationOrigin>,
}

/// The replication origin tag on a replicated document.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicationOrigin {
    /// The revision height.
    #[serde(rename = "_rev")]
    pub rev: u32,
    /// The revision hash half.
    pub hash: String,
}

/// A document in the replication meta instance.
///
/// Two kinds share this shape: per-item assumptions (`isCheckpoint: "0"`) and
/// per-direction checkpoints (`isCheckpoint: "1"`). Use
/// [`MetaDocument::is_checkpoint`] to tell them apart.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaDocument {
    /// The composed primary key, `<itemId>|<isCheckpoint>` when not compressed.
    pub id: String,
    /// The tracked item id: a document id, or `down` / `up` for checkpoints.
    #[serde(rename = "itemId")]
    pub item_id: String,
    /// `"0"` for a per-item assumption, `"1"` for a checkpoint. A string, not a
    /// boolean, because the composed primary key is a string concat.
    #[serde(rename = "isCheckpoint")]
    pub is_checkpoint: String,
    /// The checkpoint payload: the id and `lwt` of the last applied document, which
    /// is where a resumed replication continues from. `null` for per-item entries.
    #[serde(rename = "checkpointData")]
    pub checkpoint_data: Option<CheckpointData>,
    /// The fork's recorded assumption about the master's document state. `null` for
    /// checkpoints.
    #[serde(rename = "docData")]
    pub doc_data: Option<serde_json::Value>,
    /// Set when the meta entry records a resolved conflict.
    #[serde(rename = "isResolvedConflict")]
    pub is_resolved_conflict: Option<String>,
}

impl MetaDocument {
    /// True when this entry is a checkpoint rather than a per-item assumption.
    pub fn is_checkpoint(&self) -> bool {
        self.is_checkpoint == "1"
    }

    /// True when this entry tracks a single document.
    pub fn is_item(&self) -> bool {
        self.is_checkpoint == "0"
    }
}

/// The payload of a checkpoint document.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointData {
    /// The primary key of the last applied document.
    pub id: String,
    /// Its `_meta.lwt`. The pair `(lwt, id)` is what makes the checkpoint resumable
    /// without skipping documents that share an lwt.
    pub lwt: f64,
}

/// The pull-batch-size boundary case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchBoundary {
    /// The scenario label.
    pub why: String,
    /// The batch size used.
    #[serde(rename = "pullBatchSize")]
    pub pull_batch_size: usize,
    /// How many documents the master started with.
    #[serde(rename = "documentsInMaster")]
    pub documents_in_master: usize,
    /// Ids the fork ended up with, in the order the query returned them.
    #[serde(rename = "forkIds")]
    pub fork_ids: Vec<String>,
    /// The final meta snapshot.
    #[serde(rename = "meta")]
    pub snapshot: StepShape,
    /// Errors emitted. Expected empty.
    pub errors: Vec<String>,
}

// The batch-boundary entry reuses the `meta` key for what is otherwise a snapshot's
// meta array. Modelled as its own type so the field name stays honest to the JSON.
/// The `meta` array of a batch-boundary entry.
///
/// Deliberately not `Deref<Target = [MetaDocument]>`: deref coercion at the call site
/// made which type was being sliced ambiguous to the compiler, and an explicit `.0`
/// says exactly what is happening.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct StepShape(pub Vec<MetaDocument>);

impl StepShape {
    /// The meta documents, as a slice.
    pub fn as_slice(&self) -> &[MetaDocument] {
        &self.0
    }
}
