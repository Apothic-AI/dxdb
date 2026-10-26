//! Differential fuzz: random operation sequences against every storage, same seeds.
//!
//! `tests/support/fuzz.rs` holds the harness; this file wires it to the storages and adds
//! the differential comparison. Read the harness module docs first — they explain why the
//! checks are invariants rather than recorded expectations, and why the model is a model of
//! the harness's own claims rather than of the storage.
//!
//! # Reading a failure
//!
//! Every failure carries the seed, the operation index and the full operation log, so it is
//! reproducible:
//!
//! ```text
//! DXDB_FUZZ_SEED=1575829538 cargo test -p dxdb-testkit --test differential_fuzz
//! ```
//!
//! # Why the differential part matters
//!
//! Two storages can each satisfy every invariant and still disagree about observable
//! behaviour — a different sort order for equal `lwt`s, a different conflict resolution for
//! one row, a different treatment of a tombstone's payload. Comparing their final states on
//! one seed turns that into a finding instead of two greens.

#![allow(clippy::bool_assert_comparison)]

mod support;

use std::collections::BTreeMap;
use std::rc::Rc;

use dxdb_storage::RxStorage;
use dxdb_testkit::util;
use support::fuzz::{self, RunOutcome};
use support::reference_storage::ReferenceStorage;

/// One storage the harness can drive, as a name plus a closure that builds it.
///
/// A closure rather than a value because several storages need a fresh backing store per
/// sequence, and because indexeddb's backing is `Rc`-shared state that must not leak between
/// sequences.
struct UnderTest {
    name: &'static str,
    make: Box<dyn Fn() -> Box<dyn StorageDriver>>,
}

/// Everything the harness needs from a storage, erased.
///
/// The harness is generic over `RxStorage`, which cannot be put in a `Vec`; this trait is the
/// smallest object-safe wrapper that lets one loop drive all of them. It is test-only and
/// deliberately tiny.
trait StorageDriver {
    /// Runs one generated sequence.
    fn run(
        &self,
        seed: u64,
        ops: &[fuzz::Op],
    ) -> Result<RunOutcome, String>;
}

/// Blanket implementation over any `RxStorage`, so a storage needs no per-type code here.
impl<S: RxStorage + 'static> StorageDriver for S {
    fn run(&self, seed: u64, ops: &[fuzz::Op]) -> Result<RunOutcome, String> {
        let mut log = Vec::new();
        futures::executor::block_on(fuzz::run_sequence(self, seed, ops, &mut log))
    }
}

/// `FilesystemRxStorage` plus the temp directory to clean up after the run.
///
/// The cleanup has to happen after the sequence, so the storage cannot be boxed on its own:
/// dropping it does not remove the redb file, and a leaked temp tree per seed across a full
/// run is tens of megabytes.
struct FuzzFilesystem {
    storage: dxdb_filesystem::FilesystemRxStorage,
    base: std::path::PathBuf,
}

impl StorageDriver for FuzzFilesystem {
    fn run(&self, seed: u64, ops: &[fuzz::Op]) -> Result<RunOutcome, String> {
        let mut log = Vec::new();
        let result = futures::executor::block_on(fuzz::run_sequence(&self.storage, seed, ops, &mut log));
        let _ = std::fs::remove_dir_all(&self.base);
        result
    }
}

/// Every storage that passes the conformance suite.
///
/// `dxdb-sqlite` and `dxdb-filesystem` are absent because they do not implement `RxStorage`
/// (see `storage_matrix.rs`); adding them here is not possible without a trait implementation,
/// and pretending otherwise would report a driver layer as a failing storage.
fn storages_under_test() -> Vec<UnderTest> {
    vec![
        UnderTest {
            name: "reference-scaffold",
            make: Box::new(|| Box::new(ReferenceStorage::new()) as Box<dyn StorageDriver>),
        },
        UnderTest {
            name: "memory",
            make: Box::new(|| Box::new(dxdb_memory::MemoryStorage::new()) as Box<dyn StorageDriver>),
        },
        UnderTest {
            name: "localstorage",
            make: Box::new(|| {
                Box::new(dxdb_localstorage::LocalStorageStorage::new(Rc::new(
                    dxdb_localstorage::MemoryKv::default(),
                ))) as Box<dyn StorageDriver>
            }),
        },
        UnderTest {
            name: "indexeddb",
            make: Box::new(|| {
                Box::new(dxdb_indexeddb::IndexedDbStorage::new(Rc::new(
                    dxdb_indexeddb::MemoryBacking::default(),
                ))) as Box<dyn StorageDriver>
            }),
        },
        UnderTest {
            name: "filesystem",
            make: Box::new(|| {
                // A fresh temp root per sequence, so one sequence's redb file cannot leak
                // state into the next. Unique per call: the harness builds one per seed.
                use std::sync::atomic::{AtomicU64, Ordering};
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .join(format!("dxdb-fuzz-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&base);
                let storage = dxdb_filesystem::FilesystemRxStorage::new(&base);
                Box::new(FuzzFilesystem { storage, base }) as Box<dyn StorageDriver>
            }),
        },
    ]
}

/// Runs every seed against `name`'s storage, collecting outcomes for the differential.
///
/// A storage that violates an invariant is recorded and **the run continues**, so one
/// storage's bug does not hide every other storage's result. The failures are aggregated and
/// reported once at the end, because "memory, indexeddb and filesystem all pass and
/// localstorage fails 1 seed" is the useful statement, and a panic on the first failure
/// produces "localstorage fails" with no context about the others.
fn run_storage(entry: &UnderTest, seeds: &[u64]) -> (Vec<RunOutcome>, Vec<String>) {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    for &seed in seeds {
        let storage = (entry.make)();
        // The generator is seeded per sequence from its own `Rng`, not the crate's global,
        // so the same seed produces byte-identical sequences for every storage. That is what
        // makes the differential comparison meaningful.
        let rng = util::Rng::new(seed);
        let ops = fuzz::generate_ops_public(&rng, fuzz::ops_per_sequence(), fuzz::id_pool());
        match storage.run(seed, &ops) {
            Ok(outcome) => outcomes.push(outcome),
            Err(report) => failures.push(report),
        }
    }
    (outcomes, failures)
}

/// Each storage satisfies every invariant on every seed.
/// Storages with a **known, reported** invariant violation.
///
/// The fuzz harness is a diagnostic: a violation is a finding to report, not a reason to
/// fail the workspace build for every lane. But silently ignoring it would make the harness
/// decorative, so the known-failure list is exact and asserted in both directions — a new
/// failure fails the build, and fixing a listed one also fails it (so the list cannot rot).
///
/// `dxdb-localstorage` is here for one reason, reported to its owner: `run_query` returns
/// nothing for any selector, so a document written successfully is invisible to `query()`.
/// The conformance matrix finds the same bug through the tombstone selectors; the fuzzer
/// finds it through every selector. Two independent instruments agreeing is the strongest
/// evidence this lane can produce, and it is why the row stays visible rather than skipped.
const KNOWN_INVARIANT_VIOLATIONS: &[(&str, &str)] = &[(
    "localstorage",
    "run_query returns no documents for any selector (same bug the conformance matrix \
     reports as impl-query-03 and impl-count-01); referenced by its owner",
)];

#[test]
fn every_storage_satisfies_the_invariants() {
    let seeds = fuzz::seeds();
    let mut report = String::new();
    let mut any_failure = false;

    for entry in storages_under_test() {
        let (outcomes, failures) = run_storage(&entry, &seeds);
        assert_eq!(
            outcomes.len() + failures.len(),
            seeds.len(),
            "`{}` produced {} results for {} seeds",
            entry.name,
            outcomes.len() + failures.len(),
            seeds.len()
        );
        let known = KNOWN_INVARIANT_VIOLATIONS
            .iter()
            .find(|(name, _)| *name == entry.name);

        if failures.is_empty() {
            println!(
                "  {:<20} {} seeds x {} ops: all invariants hold",
                entry.name,
                seeds.len(),
                fuzz::ops_per_sequence()
            );
            if let Some((_, reason)) = known {
                any_failure = true;
                report.push_str(&format!(
                    "\n\n=== `{}` now passes ===\nIt is listed in \
                     KNOWN_INVARIANT_VIOLATIONS as: {reason}\nRemove the entry so the \
                     harness keeps asserting it.\n",
                    entry.name
                ));
            }
        } else if let Some((_, reason)) = known {
            println!(
                "  {:<20} {} of {} seeds violated (known: {})",
                entry.name,
                failures.len(),
                seeds.len(),
                reason
            );
        } else {
            any_failure = true;
            println!(
                "  {:<20} {} of {} seeds violated an invariant",
                entry.name,
                failures.len(),
                seeds.len()
            );
            for failure in &failures {
                report.push_str(&format!(
                    "\n\n=== `{}` ===\n{failure}\n",
                    entry.name
                ));
            }
        }
    }

    assert!(
        !any_failure,
        "the fuzz harness found invariant violations. Each report below names the seed, the \
         operation index and the full operation log; reproduce one with \
         DXDB_FUZZ_SEED=<seed>.\n{report}"
    );
}

/// The same seed produces the same **observable final state** on every storage.
///
/// This is the differential check. Each storage already passed its own invariant check above,
/// so a divergence here is not "one of them lost a write" — it is "two correct-by-invariant
/// storages disagree", which is a finding in its own right: either one is wrong in a way the
/// invariants do not capture, or the contract is ambiguous where they disagree.
#[test]
fn all_storages_agree_on_final_state() {
    let seeds = fuzz::seeds();
    let storages = storages_under_test();

    // Run each storage once, keeping outcomes indexed by seed.
    let mut by_storage: Vec<(&str, BTreeMap<u64, RunOutcome>)> = Vec::new();
    let mut invariant_failures = Vec::new();
    for entry in &storages {
        let (outcomes, failures) = run_storage(entry, &seeds);
        for failure in failures {
            invariant_failures.push(format!("`{}`: {failure}", entry.name));
        }
        by_storage.push((
            entry.name,
            outcomes.into_iter().map(|o| (o.seed, o)).collect(),
        ));
    }
    // A storage that violates an invariant has no comparable final state for those seeds,
    // so the differential cannot speak for it. Known ones are skipped with the reason; an
    // unknown one aborts, because a divergence nobody has explained is the finding.
    let unexplained: Vec<&String> = invariant_failures
        .iter()
        .filter(|failure| {
            !KNOWN_INVARIANT_VIOLATIONS
                .iter()
                .any(|(name, _)| failure.starts_with(&format!("`{name}`")))
        })
        .collect();
    assert!(
        unexplained.is_empty(),
        "the differential cannot run while storages violate invariants (see \
         `every_storage_satisfies_the_invariants` for the reports):\n{}",
        unexplained
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut divergences = Vec::new();
    for &seed in &seeds {
        // The reference scaffold is the baseline: it is the simplest implementation and it
        // passes the conformance suite, so a storage that disagrees with it is the one to
        // look at first.
        let Some((_, baseline)) = by_storage.first() else {
            continue;
        };
        let Some(expected) = baseline.get(&seed) else {
            continue;
        };
        for (name, outcomes) in by_storage.iter().skip(1) {
            let Some(actual) = outcomes.get(&seed) else {
                continue;
            };
            if !same_observable_state(expected, actual) {
                divergences.push(format!(
                    "seed {seed}: `{name}` disagrees with `{}`\n  \
                     live ids: {:?} vs {:?}\n  \
                     all ids:  {:?} vs {:?}\n  \
                     values:   {:?} vs {:?}",
                    by_storage[0].0,
                    expected.live_ids,
                    actual.live_ids,
                    expected.all_ids,
                    actual.all_ids,
                    expected.values,
                    actual.values,
                ));
            }
        }
    }

    assert!(
        divergences.is_empty(),
        "storages disagree on final state for {} seed(s):\n\n{}\n\n\
         Each storage passed its own invariant check, so this is either a wrong storage in a \
         way the invariants do not capture, or an ambiguous part of the contract. Reproduce a \
         seed with DXDB_FUZZ_SEED=<seed>.",
        divergences.len(),
        divergences.join("\n\n")
    );
}

/// True when two outcomes are the same in everything a caller can observe.
///
/// Revisions are compared by **height**, not by token: a token is the database instance id and
/// two storages are legitimately allowed to mint different ones. Height is the part the
/// monotonicity contract is about, so that is the part a differential compares.
fn same_observable_state(a: &RunOutcome, b: &RunOutcome) -> bool {
    if a.live_ids != b.live_ids || a.all_ids != b.all_ids || a.values != b.values {
        return false;
    }
    let heights_a: BTreeMap<&String, u32> = a
        .revisions
        .iter()
        .map(|(id, rev)| (id, dxdb_storage::get_height_of_revision(rev)))
        .collect();
    let heights_b: BTreeMap<&String, u32> = b
        .revisions
        .iter()
        .map(|(id, rev)| (id, dxdb_storage::get_height_of_revision(rev)))
        .collect();
    heights_a == heights_b
}

/// The harness must be able to fail: a storage that loses writes must be caught.
///
/// Without this, a harness whose assertions quietly stopped asserting would stay green, and
/// every row above would be meaningless. This is the same anti-vacuity discipline the
/// conformance suite uses, applied to the fuzzer.
#[test]
fn the_harness_catches_a_storage_that_loses_writes() {
    use dxdb_storage::{
        BulkWriteResponse, ChangedDocuments, ChangeStream, Checkpoint, CountResult,
        DocumentData, DynRxStorageInstance, FindAttachmentResult, InstanceCreationParams,
        PreparedQuery, RxStorage, RxStorageInstance, StorageError, StorageSchema,
    };

    /// Accepts every write, stores only the first document, and reports success for all.
    /// A storage that loses writes is the exact failure the "no lost writes" invariant is for.
    struct LossyStorage;
    struct LossyInstance {
        database_name: String,
        collection_name: String,
        schema: StorageSchema,
        stored: std::cell::RefCell<Vec<DocumentData>>,
    }

    impl RxStorage for LossyStorage {
        fn name(&self) -> &str {
            "lossy"
        }
        async fn create_storage_instance(
            &self,
            params: InstanceCreationParams,
        ) -> Result<Box<DynRxStorageInstance<'static>>, StorageError> {
            Ok(DynRxStorageInstance::new_box(LossyInstance {
                database_name: params.database_name,
                collection_name: params.collection_name,
                schema: params.schema,
                stored: std::cell::RefCell::new(Vec::new()),
            }))
        }
    }

    impl RxStorageInstance for LossyInstance {
        fn database_name(&self) -> &str {
            &self.database_name
        }
        fn collection_name(&self) -> &str {
            &self.collection_name
        }
        fn schema(&self) -> &StorageSchema {
            &self.schema
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn bulk_write(
            &self,
            rows: Vec<dxdb_storage::BulkWriteRow>,
            _context: String,
        ) -> Result<BulkWriteResponse, StorageError> {
            // Store only the first row of each call and silently drop the rest.
            if let Some(row) = rows.into_iter().next() {
                self.stored
                    .borrow_mut()
                    .push(row.document.into_document_data());
            }
            Ok(BulkWriteResponse::ok())
        }
        async fn find_documents_by_id(
            &self,
            ids: Vec<String>,
            _with_deleted: bool,
        ) -> Result<Vec<DocumentData>, StorageError> {
            let primary = self.schema.primary_path.clone();
            Ok(self
                .stored
                .borrow()
                .iter()
                .filter(|doc| {
                    doc.id(&primary)
                        .map(|id| ids.iter().any(|wanted| wanted == id))
                        .unwrap_or(false)
                })
                .cloned()
                .collect())
        }
        async fn query(
            &self,
            _prepared: PreparedQuery,
        ) -> Result<Vec<DocumentData>, StorageError> {
            Ok(self.stored.borrow().clone())
        }
        async fn count(&self, prepared: PreparedQuery) -> Result<CountResult, StorageError> {
            Ok(CountResult {
                count: self.query(prepared).await?.len(),
                mode: dxdb_storage::CountMode::Slow,
            })
        }
        async fn get_attachment_data(
            &self,
            _document_id: String,
            _attachment_id: String,
            _digest: String,
        ) -> Result<FindAttachmentResult, StorageError> {
            Err(StorageError::MissingAttachment("lossy".to_string()))
        }
        async fn get_changed_documents_since(
            &self,
            _limit: usize,
            _checkpoint: Option<Checkpoint>,
        ) -> Result<ChangedDocuments, StorageError> {
            Ok(ChangedDocuments {
                documents: Vec::new(),
                checkpoint: Checkpoint::beginning(),
            })
        }
        fn change_stream(&self) -> ChangeStream {
            Box::pin(futures::stream::empty())
        }
        async fn cleanup(&self, _minimum_deleted_time: f64) -> Result<bool, StorageError> {
            Ok(true)
        }
        async fn close(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn remove(&self) -> Result<(), StorageError> {
            Ok(())
        }
    }

    // Run a sequence that writes several documents; the lossy storage must be caught.
    let seed = fuzz::seeds().first().copied().unwrap_or(1);
    let rng = util::Rng::new(seed);
    let ops = fuzz::generate_ops_public(&rng, fuzz::ops_per_sequence(), fuzz::id_pool());

    let mut log = Vec::new();
    let outcome = futures::executor::block_on(fuzz::run_sequence(
        &LossyStorage,
        seed,
        &ops,
        &mut log,
    ));
    assert!(
        outcome.is_err(),
        "the fuzz harness passed a storage that silently drops every write after the first \
         in each call. Every other assertion in this file is now suspect: a harness that \
         cannot fail is not a harness."
    );
}
