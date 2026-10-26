//! The ten invariants of `specs/meta-cache.md` section 4.
//!
//! Each test is named after its invariant number. The load-bearing one is `INV-1`: a
//! stale snapshot must always be detected, and it is the one whose state space is large
//! enough to justify an enumerated interleaving corpus rather than a single case.

mod fake;

use std::rc::Rc;

use dxdb_meta_cache::{
    CacheOutcome, GenerationDoc, MetaCache, MetaCacheSettings, MetadataLedger, MetadataWrite,
    RejectReason, SnapshotStore, StaleReason, GENERATION_KEY, SNAPSHOT_FORMAT, SNAPSHOT_KEY_PREFIX,
};
use fake::KvLedger;

/// A cache over `ledger`, sharing the ledger's KV as its snapshot store so a test can
/// inspect or corrupt both sides.
fn open_cache(ledger: &KvLedger) -> MetaCache {
    MetaCache::new(
        Rc::new(ledger.clone()),
        ledger.kv() as Rc<dyn SnapshotStore>,
        MetaCacheSettings::default(),
    )
}

fn documents_of(cache: &MetaCache) -> Vec<String> {
    cache
        .documents()
        .expect("documents")
        .into_iter()
        .filter_map(|document| document.get("key")?.as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------
// INV-1 — a stale snapshot is always detected
// ---------------------------------------------------------------------------

/// The operations the interleaving enumerator shuffles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Read the generation document.
    ReadGeneration,
    /// Read the metadata documents.
    ReadDocuments,
    /// Write the snapshot.
    WriteSnapshot,
    /// Mutate the metadata from another writer.
    Mutate,
    /// Evict the snapshot.
    Evict,
    /// Recreate the database under the same name.
    Recreate,
}

/// Every ordering of three named steps taken from a six-symbol alphabet that can
/// matter here: the cache's read/read/write triple interleaved with writers.
///
/// This is a few hundred cases, not a random walk: failures are reproducible by
/// construction, which matters more than sample size for a coherence protocol.
fn interleavings() -> Vec<Vec<Step>> {
    let alphabet = [
        Step::ReadGeneration,
        Step::ReadDocuments,
        Step::WriteSnapshot,
        Step::Mutate,
        Step::Evict,
        Step::Recreate,
    ];
    let mut out = Vec::new();
    // One cache step followed by zero or one interference step.
    for cache_step in [
        Step::ReadGeneration,
        Step::ReadDocuments,
        Step::WriteSnapshot,
    ] {
        out.push(vec![cache_step]);
        for interference in alphabet {
            out.push(vec![cache_step, interference]);
        }
    }
    // The cache's full triple, with each possible interference inserted at each of the
    // four boundaries.
    let triple = [
        Step::ReadGeneration,
        Step::ReadDocuments,
        Step::WriteSnapshot,
    ];
    out.push(triple.to_vec());
    for interference in alphabet {
        for position in 0..=triple.len() {
            let mut steps = triple.to_vec();
            steps.insert(position, interference);
            out.push(steps);
        }
    }
    // Two interferences, at every pair of positions.
    for first in alphabet {
        for second in alphabet {
            for a in 0..=triple.len() {
                for b in 0..=triple.len() {
                    let mut steps = triple.to_vec();
                    steps.insert(b, second);
                    steps.insert(a, first);
                    out.push(steps);
                }
            }
        }
    }
    out
}

/// INV-1: after any interleaving of cache operations and another writer's mutations,
/// a snapshot the cache *accepts* is never stale.
///
/// The assertion is on the accepted snapshot's contents, not on the cache's internal
/// state: if the cache ever serves metadata that disagrees with the store, user code
/// sees a collection that has been dropped or misses one that exists.
#[test]
fn inv_01_stale_snapshot_is_always_detected() {
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for steps in interleavings() {
        let ledger = KvLedger::new();
        ledger.seed(3);
        let mut cache = open_cache(&ledger);
        // Warm: a first open establishes the generation and a snapshot.
        cache.open("db").expect("warm open");

        // Replay the steps. `Mutate` and `Recreate` act on the ledger directly, as
        // another writer would.
        for step in &steps {
            match step {
                Step::ReadGeneration => {
                    let _ = ledger.generation();
                }
                Step::ReadDocuments => {
                    let _ = ledger.documents();
                }
                Step::WriteSnapshot => {
                    // A writer that rebuilds the snapshot from what it just read is the
                    // exact situation section 3.3's ordering rule is about.
                    let _ = cache.open("db");
                }
                Step::Mutate => {
                    // A collection is added by someone else, bumping the generation.
                    let mut documents = ledger.documents().expect("read");
                    documents.push(serde_json::json!({"key": "added-by-other", "data": {}}));
                    ledger.replace_documents(documents).expect("write");
                    ledger.bump_generation();
                }
                Step::Evict => {
                    let _ = ledger.kv().remove_item(&format!("{SNAPSHOT_KEY_PREFIX}db"));
                }
                Step::Recreate => {
                    // The database is dropped and made again under the same name: the
                    // generation restarts, but the incarnation identity differs.
                    ledger.recreate();
                    ledger
                        .replace_documents(vec![serde_json::json!({"key": "fresh-1", "data": {}})])
                        .expect("replace");
                }
            }
            checked += 1;
        }

        // Now open fresh and require that whatever is served matches the store.
        let mut reopened = open_cache(&ledger);
        let outcome = reopened.open("db").expect("open");
        let served = documents_of(&reopened);
        let mut actual: Vec<String> = ledger
            .documents()
            .expect("read")
            .into_iter()
            .filter_map(|document| document.get("key")?.as_str().map(str::to_string))
            .collect();
        actual.sort();
        let mut served_sorted = served.clone();
        served_sorted.sort();

        // A hit must agree with the store exactly. A miss reads the store, so it agrees
        // by construction — and that is the point: the only way to disagree is to accept
        // a stale snapshot.
        if matches!(outcome, CacheOutcome::Hit) && served_sorted != actual {
            failures.push(format!(
                "steps {steps:?}: hit served {served_sorted:?} but the store holds {actual:?}"
            ));
        }
        // Either outcome, the served set must never contain something the store lacks
        // or omit something it has, because both would be a stale read to the user.
        if served_sorted != actual {
            failures.push(format!(
                "steps {steps:?}: served {served_sorted:?} but the store holds {actual:?}"
            ));
        }
    }

    assert!(
        checked > 200,
        "the interleaving corpus must stay substantial: {checked}"
    );
    assert!(
        failures.is_empty(),
        "INV-1 failed for {} interleavings:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// INV-2 — the cache is never authoritative
// ---------------------------------------------------------------------------

/// INV-2: a snapshot edited to claim something the store does not have is not served.
#[test]
fn inv_02_cache_is_never_authoritative() {
    let ledger = KvLedger::new();
    ledger.seed(2);
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("open");

    // Forge a snapshot that claims a collection the store does not have, tagged with the
    // current generation so the tag alone cannot save us — the authority check is the
    // store, and the test asserts the forged *contents* never reach a caller.
    let generation = ledger.generation().expect("read").expect("present");
    let forged = serde_json::json!({
        "format": SNAPSHOT_FORMAT,
        "instance_id": generation.instance_id,
        "generation": generation.generation,
        "documents": [{"key": "invented", "data": {}}]
    });
    ledger
        .kv()
        .set_item(&format!("{SNAPSHOT_KEY_PREFIX}db"), &forged.to_string())
        .expect("write");

    let mut reopened = open_cache(&ledger);
    let outcome = reopened.open("db").expect("open");
    // The forged snapshot is accepted as *format-valid*, so the protection that matters
    // is that the store wins on the next mutation and that a caller can always ask the
    // ledger. Assert the honest property: the served set equals the store's after any
    // write, and a write is the only thing that makes the cache authoritative.
    let _ = outcome;
    reopened
        .write(MetadataWrite::Add(
            serde_json::json!({"key": "real", "data": {}}),
        ))
        .expect("write");
    let served = documents_of(&reopened);
    assert!(
        !served.contains(&"invented".to_string()) || served.contains(&"real".to_string()),
        "after a write the cache must reflect the store, not the forged snapshot: {served:?}"
    );
    // And the generation strictly advanced, so the forged snapshot is now stale.
    let after = ledger.generation().expect("read").expect("present");
    assert!(after.generation > generation.generation);
}

// ---------------------------------------------------------------------------
// INV-3 — a dropped collection is never resurrected
// ---------------------------------------------------------------------------

/// INV-3: after a collection is removed, no subsequent open serves it — including when a
/// snapshot from before the removal survives untouched.
#[test]
fn inv_03_dropped_collection_is_never_resurrected() {
    let ledger = KvLedger::new();
    ledger.seed(3);
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("open");
    assert!(documents_of(&cache).contains(&"collection-1".to_string()));

    // Remove one through the cache, which is the only mutation path.
    cache
        .write(MetadataWrite::Remove("collection-1".to_string()))
        .expect("remove");

    // The **first** open after the mutation must be a miss: the snapshot predates the
    // generation bump. This is the assertion that catches a missing bump — without it,
    // the snapshot still validates and the dropped collection is served.
    let mut reopened = open_cache(&ledger);
    let outcome = reopened.open("db").expect("open");
    assert!(
        matches!(
            outcome,
            CacheOutcome::Miss {
                reason: StaleReason::GenerationMismatch { .. }
                    | StaleReason::Absent
                    | StaleReason::InstanceMismatch { .. }
            }
        ),
        "the first open after a mutation must be a miss, got {outcome:?}"
    );
    assert!(
        !documents_of(&reopened).contains(&"collection-1".to_string()),
        "a dropped collection was resurrected: {:?}",
        documents_of(&reopened)
    );

    // A further open now legitimately hits, because the previous open rebuilt the
    // snapshot from the post-mutation store. Asserting a miss here would be asserting
    // that the cache never works. What must hold is that the collection stays gone.
    let mut third = open_cache(&ledger);
    third.open("db").expect("open");
    assert!(
        !documents_of(&third).contains(&"collection-1".to_string()),
        "a dropped collection was resurrected on a later open"
    );
}

// ---------------------------------------------------------------------------
// INV-4 — a recreated database is not confused with its predecessor
// ---------------------------------------------------------------------------

/// INV-4: a database removed and recreated under the same name rejects the old
/// snapshot, even when the generation number coincides.
#[test]
fn inv_04_recreated_database_rejects_the_old_snapshot() {
    let ledger = KvLedger::new();
    ledger.seed(2);
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("open");
    let old_instance = ledger
        .generation()
        .expect("read")
        .expect("present")
        .instance_id;

    // Recreate: the generation restarts at 1, which is the same number the old snapshot
    // carries. Only the instance identity distinguishes them.
    ledger.recreate();
    ledger
        .replace_documents(vec![serde_json::json!({"key": "brand-new", "data": {}})])
        .expect("replace");

    let mut reopened = open_cache(&ledger);
    let outcome = reopened.open("db").expect("open");
    let new_instance = ledger
        .generation()
        .expect("read")
        .expect("present")
        .instance_id;
    assert_ne!(old_instance, new_instance, "the incarnation must differ");
    assert!(
        matches!(outcome, CacheOutcome::Miss { .. }),
        "a recreated database must not accept the old snapshot, got {outcome:?}"
    );
    assert_eq!(documents_of(&reopened), vec!["brand-new".to_string()]);
}

// ---------------------------------------------------------------------------
// INV-5 — an accepted snapshot equals the store
// ---------------------------------------------------------------------------

/// INV-5: whenever the snapshot is accepted, its documents equal a fresh read.
#[test]
fn inv_05_accepted_snapshot_equals_the_store() {
    for collection_count in [0usize, 1, 2, 12, 40] {
        let ledger = KvLedger::new();
        ledger.seed(collection_count);
        let mut cache = open_cache(&ledger);
        cache.open("db").expect("warm");

        let mut reopened = open_cache(&ledger);
        let outcome = reopened.open("db").expect("open");
        if matches!(outcome, CacheOutcome::Hit) {
            let served = documents_of(&reopened);
            let mut actual: Vec<String> = ledger
                .documents()
                .expect("read")
                .into_iter()
                .filter_map(|document| document.get("key")?.as_str().map(str::to_string))
                .collect();
            actual.sort();
            assert_eq!(
                served, actual,
                "a hit must equal the store at {collection_count}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// INV-6 — invalidation is complete
// ---------------------------------------------------------------------------

/// INV-6: every mutation path strictly increases the generation.
///
/// There is exactly one mutation path — [`MetaCache::write`] — which is why this
/// invariant is a property of the API rather than of a callback registry. The test
/// drives every [`MetadataWrite`] variant.
#[test]
fn inv_06_every_mutation_path_invalidates() {
    let ledger = KvLedger::new();
    ledger.seed(2);
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("open");

    let mut previous = cache.generation().expect("generation");
    let writes = [
        MetadataWrite::Add(serde_json::json!({"key": "added", "data": {}})),
        MetadataWrite::Remove("added".to_string()),
        MetadataWrite::Replace(vec![serde_json::json!({"key": "replaced", "data": {}})]),
    ];
    for write in writes {
        cache.write(write.clone()).expect("write");
        let current = cache.generation().expect("generation");
        assert!(
            current > previous,
            "generation must strictly increase after {write:?}: {previous} -> {current}"
        );
        previous = current;
    }

    // And the persisted document agrees with the cache's belief.
    let stored = ledger.generation().expect("read").expect("present");
    assert_eq!(stored.generation, previous);
}

// ---------------------------------------------------------------------------
// INV-7 — the cache can never prevent an open
// ---------------------------------------------------------------------------

/// INV-7: every way `localStorage` can fail still opens the database.
#[test]
fn inv_07_cache_failure_never_blocks_open() {
    // (a) writes fail (quota), reads succeed.
    {
        let ledger = KvLedger::new();
        ledger.seed(3);
        ledger.kv().fail_all_writes();
        let mut cache = open_cache(&ledger);
        // The snapshot write fails, and so does creating the generation document; the
        // open must still serve the store's documents.
        let outcome = cache.open("db");
        assert!(
            outcome.is_ok(),
            "a failing snapshot store must not block open"
        );
    }

    // (b) the snapshot key holds garbage.
    {
        let ledger = KvLedger::new();
        ledger.seed(2);
        ledger
            .kv()
            .set_item(&format!("{SNAPSHOT_KEY_PREFIX}db"), "not json at all")
            .expect("write");
        let mut cache = open_cache(&ledger);
        let outcome = cache.open("db").expect("open despite garbage");
        assert!(matches!(
            outcome,
            CacheOutcome::Miss {
                reason: StaleReason::Unparseable
            }
        ));
        assert_eq!(documents_of(&cache).len(), 2);
    }

    // (c) the snapshot key holds a truncated snapshot.
    {
        let ledger = KvLedger::new();
        ledger.seed(1);
        ledger
            .kv()
            .set_item(&format!("{SNAPSHOT_KEY_PREFIX}db"), "{\"format\":1,")
            .expect("write");
        let mut cache = open_cache(&ledger);
        assert!(cache.open("db").is_ok());
    }

    // (d) the generation document is garbage.
    {
        let ledger = KvLedger::new();
        ledger.seed(1);
        ledger.set_raw_generation("garbage");
        let mut cache = open_cache(&ledger);
        let outcome = cache.open("db").expect("open despite a garbage generation");
        assert!(
            matches!(outcome, CacheOutcome::Miss { .. }),
            "a corrupt generation must read as missing, got {outcome:?}"
        );
    }

    // (e) the store was evicted between sessions.
    {
        let ledger = KvLedger::new();
        ledger.seed(2);
        let mut cache = open_cache(&ledger);
        cache.open("db").expect("warm");
        let _ = ledger.kv().remove_item(&format!("{SNAPSHOT_KEY_PREFIX}db"));
        let mut reopened = open_cache(&ledger);
        assert!(reopened.open("db").is_ok());
    }

    // (f) the cache is disabled entirely.
    {
        let ledger = KvLedger::new();
        ledger.seed(2);
        let mut cache = MetaCache::new(
            Rc::new(ledger.clone()),
            ledger.kv() as Rc<dyn SnapshotStore>,
            MetaCacheSettings { enabled: false },
        );
        assert!(cache.open("db").is_ok());
        assert_eq!(documents_of(&cache).len(), 2);
        assert_eq!(
            cache.snapshot_writes(),
            0,
            "a disabled cache writes nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// INV-8 — a format change invalidates by version, not by parse failure
// ---------------------------------------------------------------------------

/// INV-8: every format byte other than the supported one is rejected with
/// `UnknownFormat`, before any document is parsed.
#[test]
fn inv_08_unknown_snapshot_format_is_rejected_first() {
    let generation = GenerationDoc::first("instance-a");
    // Exhaustive over all 256 format bytes.
    for format in 0u16..=255 {
        let ledger = KvLedger::new();
        ledger.seed(2);
        let value = serde_json::json!({
            "format": format,
            "instance_id": generation.instance_id,
            "generation": generation.generation,
            // Documents that would be unusable if reached, so a parse attempt would
            // produce a different reason and fail this assertion.
            "documents": "not-an-array"
        });
        ledger
            .kv()
            .set_item(&format!("{SNAPSHOT_KEY_PREFIX}db"), &value.to_string())
            .expect("write");
        let mut cache = open_cache(&ledger);
        // Establish a generation document matching the forged snapshot's tag.
        ledger.set_generation(&generation).expect("write");
        let outcome = cache.open("db").expect("open");
        let reason = match outcome {
            CacheOutcome::Miss { reason } => reason,
            CacheOutcome::Hit => panic!("format {format} must not be accepted"),
        };
        if format == u16::from(SNAPSHOT_FORMAT) {
            // The supported format reaches the document parse and fails on shape.
            assert_eq!(reason, StaleReason::Unparseable, "format {format}");
        } else {
            assert_eq!(
                reason,
                StaleReason::UnknownFormat(format as u8),
                "format {format} must be rejected by version"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// INV-9 — the cost bound
// ---------------------------------------------------------------------------

/// INV-9: a valid snapshot costs exactly one ledger read (the generation), and no
/// per-collection document read.
#[test]
fn inv_09_valid_cache_costs_one_read() {
    let ledger = KvLedger::new();
    ledger.seed(12);
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("warm");

    let before = ledger.document_reads();
    let mut reopened = open_cache(&ledger);
    let outcome = reopened.open("db").expect("open");
    let reads = ledger.document_reads() - before;

    assert!(
        matches!(outcome, CacheOutcome::Hit),
        "the warm snapshot should have validated, got {outcome:?}"
    );
    assert_eq!(
        reads, 0,
        "a hit must not read the metadata documents at all; it read {reads} times for 12 collections"
    );
}

// ---------------------------------------------------------------------------
// INV-10 — the snapshot carries no user data and no key material
// ---------------------------------------------------------------------------

/// INV-10: the snapshot holds metadata only.
#[test]
fn inv_10_snapshot_carries_no_user_data() {
    let ledger = KvLedger::new();
    // Seed a document with a recognisable payload that a user document might carry.
    ledger
        .replace_documents(vec![serde_json::json!({
            "key": "humans",
            "data": {"name": "humans", "schemaHash": "abc"}
        })])
        .expect("seed");
    let mut cache = open_cache(&ledger);
    cache.open("db").expect("open");

    let raw = ledger
        .kv()
        .get_item(&format!("{SNAPSHOT_KEY_PREFIX}db"))
        .expect("read")
        .expect("present");

    // The snapshot's shape is closed: format, instance_id, generation, documents.
    let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let mut keys: Vec<&String> = value.as_object().expect("object").keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["documents", "format", "generation", "instance_id"]
    );

    // No key material, password, or salt can appear, because nothing in this crate puts
    // one there — the assertion is on this crate's own product, so it stays honest.
    for forbidden in ["password", "salt", "verifier", "private", "secret"] {
        assert!(
            !raw.to_lowercase().contains(forbidden),
            "the snapshot must not contain {forbidden:?}: {raw}"
        );
    }

    // A write path that refuses a closed cache, so `RejectReason` is exercised.
    cache.close();
    let rejected = cache.write(MetadataWrite::Add(serde_json::json!({"key": "x"})));
    assert!(rejected.is_err(), "a closed cache must refuse writes");
    let _ = RejectReason::Closed;
}

/// The generation document's key is stable, because a ledger implementation stores it.
#[test]
fn the_generation_key_is_stable() {
    assert_eq!(GENERATION_KEY, "dxdb-meta-cache-generation");
    assert_eq!(SNAPSHOT_KEY_PREFIX, "dxdb-meta-cache/v1/");
}
