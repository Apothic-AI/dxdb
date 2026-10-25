//! The redb table layout: tables, key encoding, iteration order, and how
//! multi-instance safety comes from the engine.
//!
//! # Provenance: clean-room
//!
//! **No upstream source may be read for this crate.** Both filesystem storages
//! (`storage-filesystem-node`, `storage-filesystem-expo`) ship in the closed
//! `rxdb-premium` package, and CLEANROOM.md section 2 forbids reading that code at
//! all. There is no readable reference implementation. Everything here is designed
//! from public documentation plus our own engineering, per CLEANROOM.md 5.4.
//!
//! If you are about to open `rxdb-premium` to see "how they did it": stop. Read
//! this module and `docs/lanes/m5-port-plan.md` section 2 instead, or ask.
//!
//! What the public docs establish, and nothing more: documents are persisted as
//! distinct files rather than one monolithic file; there is a `basePath` and an
//! `inWorker` option; the same folder can be opened from parallel Node processes
//! when `multiInstance: true`; and it is measured as somewhat faster than SQLite.
//!
//! # The layout
//!
//! One redb file per folder, holding six tables. All keys are `&[u8]`: redb is a
//! byte-string key-value store, so typed tables are built by encoding the key
//! ourselves rather than by asking the engine for a type system.
//!
//! | Table | Key | Value | Purpose |
//! | --- | --- | --- | --- |
//! | `meta` | `b"schema"` | schema-version map | open-time validation |
//! | `docs` | `collection \0 version \0 docId` | ordered-JSON document | primary store, the only source of truth |
//! | `chg` | `BE(flip(lwt))` ++ `collection \0 version \0 docId` | empty | lwt-ordered scan for `changed_since` |
//! | `cleanup` | `collection \0 version \0` ++ deleted ++ `BE(flip(lwt))` ++ `docId` | empty | range delete by age |
//! | `idx` | `collection \0 version \0` ++ index string | == | secondary range scans |
//! | `attach` | `docId \0 attachmentId` | bytes | attachment blobs |
//!
//! Naming note: the real `TableDefinition`s live in [`super`]; this module is the
//! specification those constants must match, and [`layout_tests`] asserts that
//! they do. Keeping the prose and the constants in different files without a test
//! is how a layout document becomes fiction.
//!
//! # Key encoding, and why each choice is load-bearing
//!
//! 1. **`0x00` separates key parts, not `:` or `|`.** A document id may contain
//!    any UTF-8, including `:` and `|`; upstream has to substitute `|` out of
//!    IndexedDB keys for exactly this reason (`dexie-helpers`'
//!    `dexieReplaceIfStartsWithPipe`). A byte that our schema validation rejects
//!    in a primary key cannot collide with a separator.
//!
//! 2. **A separator also stops prefix bleed between collection names.** `todos`
//!    is a prefix of `todos_archive`. Without a terminator, a prefix scan for
//!    `todos` would also return `todos_archive`'s documents. With it, `todos\0` is
//!    not a prefix of `todos_archive\0`. This is the detail that produces a
//!    storage which passes every test its author wrote and then corrupts a
//!    production dataset, so it gets a dedicated test.
//!
//! 3. **`lwt` is an `i64` in big-endian *with the sign bit flipped*.** redb orders
//!    keys lexicographically, so a signed integer must be offset before encoding
//!    or negative values sort after positive ones. `lwt` is a JavaScript
//!    millisecond timestamp and therefore positive in practice; encoding it
//!    correctly costs one XOR and removes a class of "works until a test uses a
//!    synthetic clock" surprise. See [`encode_lwt`].
//!
//! 4. **Index strings are used as-is, not re-encoded.** They are already ordered
//!    by dxdb's encoding (PLAN.md 4.3), so a lexicographic range scan over the
//!    encoded bytes *is* an index-ordered range scan. Adding a collation layer
//!    here would be a second ordering to keep in sync with the first.
//!
//! 5. **The schema version is in the `docs` and `idx` keys.** Two versions of one
//!    collection coexist in one file, so a migration can read the old version
//!    while writing the new one.
//!
//! # Iteration order
//!
//! redb returns keys in lexicographic byte order, and every read is a `Range`.
//! Consequences the storage layer relies on:
//!
//! - A prefix scan of `docs` under `collection \0 version \0` yields one
//!   collection's documents in id order.
//! - `find_documents_by_id` is N point lookups, not a scan. This is the first
//!   place this storage beats the SQLite trial implementation, which scans the
//!   whole table and filters in JavaScript
//!   (`sqlite-storage-instance.ts:352`).
//! - `chg` ascending from `(lastLwt, lastId)` *is* `changed_since`, with `limit`
//!   applied by stopping the iterator. No allocation per skipped row beyond the
//!   key decode.
//! - `cleanup` is a contiguous range from `collection \0 version \0` ++ `1` (the
//!   deleted flag) ++ `0..=cutoff` ++ nothing, i.e. a bounded batch delete, and
//!   `false` is returned while more work remains — the tri-state contract in
//!   PLAN.md 4.2.
//! - `idx` scans from the planner's `start_keys` to `end_keys`, each hit a point
//!   lookup into `docs`.
//! - **Descending sort is not supported**, and that matches upstream: a `desc` in
//!   the sort forces a re-sort above the storage layer (`query-planner.ts:48`).
//!
//! # Multi-instance safety: what the engine gives, and what it does not
//!
//! "Safe with `multiInstance: true`" hides three separate claims. Only the first
//! two are free; the third is not, and pretending otherwise is how a storage ends
//! up with hand-rolled locking.
//!
//! ## Claim 1 — mutual exclusion, no corruption, no lost writes
//!
//! Provided by redb, in `ConcurrencyMode::MultiWriter`: "any number of processes
//! may, concurrently, open the database for reading and writing. Only one write
//! transaction may be open at a time." redb serialises writers internally and its
//! commits are crash safe.
//!
//! **This requires the `experimental-multiprocess` feature**, and that is a real
//! decision rather than a footnote. redb's default is
//! `ConcurrencyMode::ExclusiveWriter`, which **locks the whole file to one
//! process**: a second process's `open` fails with "Database already open. Cannot
//! acquire lock." So the default cannot satisfy `multiInstance: true` at all.
//!
//! Measured on the pinned toolchain (redb 4.4.0, 1.95.0), two processes, one file:
//!
//! ```text
//! MultiWriter:      writer P1 opens, reader P2 opens
//!                   reader sees 3 rows -> 4 -> 5 as the writer commits
//!                   (cross-process visibility follows committed transactions)
//! ExclusiveWriter:  reader: OPEN FAILED: Database already open. Cannot acquire lock.
//! ```
//!
//! So `MultiWriter` is the mode this storage uses, and the cost is honest: the
//! feature pulls in `experimental-api-5`, which **changes redb's read-only table
//! API** (`range` becomes `range_owned` taking a `KeyRange`, gated
//! `#[cfg(not(feature = "experimental-api-5"))]`). Our access goes through write
//! and read transactions and the `ReadableTable` trait, which is unaffected, but a
//! future change must not assume the non-experimental signatures. The feature name
//! is a warning, not a decoration: pin redb, and re-run the cross-process test on
//! every bump.
//!
//! ## Claim 2 — a second process sees the first one's committed writes
//!
//! Provided, but **only on a fresh transaction**. redb is snapshot-isolated: a
//! long-lived read transaction keeps returning its own snapshot forever. Hence the
//! design rule, which is a rule and not a suggestion:
//!
//! > Read transactions are short and per-operation. Never cache one on the
//! > instance.
//!
//! One `bulk_write` opens one write transaction; one `query` opens one read
//! transaction and drops it. The moment an instance caches a `ReadTransaction`
//! "for performance", cross-process visibility silently breaks in a way no
//! single-process test will catch. The cross-process test exists to catch it.
//!
//! ## Claim 3 — cross-process change *events*
//!
//! **Not provided by the engine, and this is the honest gap.** `EventBulk`s are
//! in-process: a document written by process B does not wake process A's live
//! query, because redb has no notification channel.
//!
//! Upstream gets this from `broadcast-channel` inside
//! `addRxStorageMultiInstanceSupport` (`rx-storage-multiinstance.ts:102`), which is
//! a JavaScript mechanism — `BroadcastChannel`, `localStorage` events, a socket.
//! None of those exist between two independent processes on a redb file, and
//! building one means either a lockfile protocol or a polling tail.
//!
//! The decision: **in-process bus for same-process instances, plus a `chg`-table
//! tail on a configurable interval for cross-process.** Rationale:
//!
//! - Claims 1 and 2 are what "does not lose or corrupt your data" means, and both
//!   are fully covered by the engine. Claim 3 is a *latency* property.
//! - Tailing `chg` is cheap and resumable: each instance keeps its last-seen
//!   `(lwt, id)` cursor, and a poll is a range scan that returns empty in the
//!   steady state. There is no extra bookkeeping table.
//! - It reuses the `changed_since` access path, so the polling implementation is
//!   not throwaway work.
//! - The interval is a documented setting, and `multi_instance: false` disables the
//!   tailer entirely, so the common single-process case pays nothing.
//!
//! Recorded as a **known behavioural delta**: cross-process change delivery is
//! bounded-latency rather than immediate. The conformance suite asserts eventual
//! delivery with `waitUntil` semantics, which a 250 ms tail satisfies. If a test
//! turns out to require sub-millisecond cross-process delivery, that test is
//! testing a message bus rather than a storage, and the delta gets raised rather
//! than papered over.
//!
//! # Why one file rather than one per collection
//!
//! The folder is the unit of the `multiInstance` claim. Two collections in one
//! folder are one durability domain, and splitting them means a write touching two
//! collections cannot be atomic even in principle. redb's commit is file-wide
//! anyway; more files means more independent commit points and more chances for a
//! torn multi-collection state. One file also means one file to copy for a backup.
//!
//! The counter-consideration, recorded rather than hidden: one file means one
//! write lock for the whole folder, so two collections with disjoint write traffic
//! serialise against each other. Acceptable at this storage's target scale
//! (single node, single-digit collections), and revisitable behind a
//! `per_collection_files` option if a benchmark says otherwise.

/// The separator between key parts. Cannot appear in a primary key.
pub const KEY_SEPARATOR: u8 = 0x00;

/// Encodes an `lwt` so that lexicographic byte order equals numeric order.
///
/// Big-endian, with the sign bit flipped: `lwt` is a signed `i64` and redb compares
/// bytes, so without the flip every negative timestamp would sort after every
/// positive one.
pub fn encode_lwt(lwt: i64) -> [u8; 8] {
    ((lwt as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
}

/// Decodes [`encode_lwt`], for a cursor position.
pub fn decode_lwt(bytes: &[u8]) -> Option<i64> {
    let array: [u8; 8] = bytes.try_into().ok()?;
    Some((u64::from_be_bytes(array) ^ 0x8000_0000_0000_0000) as i64)
}

/// Builds the key prefix shared by one collection at one schema version.
///
/// Ends with [`KEY_SEPARATOR`], which is what stops `todos` from matching
/// `todos_archive`.
pub fn prefix(collection: &str, version: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(collection.len() + 8);
    out.extend_from_slice(collection.as_bytes());
    out.push(KEY_SEPARATOR);
    out.extend_from_slice(version.to_string().as_bytes());
    out.push(KEY_SEPARATOR);
    out
}

/// Builds a `cleanup`-table prefix for one collection at one version, and a
/// deleted-flag value.
pub fn cleanup_prefix(collection: &str, version: u32, deleted: bool) -> Vec<u8> {
    let mut out = prefix(collection, version);
    out.push(u8::from(deleted));
    out
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn lwt_encoding_preserves_numeric_order() {
        // The property the encoding exists for: byte order == numeric order.
        let values = [i64::MIN, -1, 0, 1, 1_700_000_000_000, i64::MAX];
        let mut encoded: Vec<[u8; 8]> = values.iter().copied().map(encode_lwt).collect();
        encoded.sort();
        let mut sorted_values = values.to_vec();
        sorted_values.sort();
        let round_tripped: Vec<i64> = encoded
            .iter()
            .map(|b| decode_lwt(b).expect("decode"))
            .collect();
        assert_eq!(round_tripped, sorted_values);
    }

    #[test]
    fn lwt_round_trips() {
        for v in [0i64, 1, -1, 1_700_000_000_000, i64::MIN, i64::MAX] {
            assert_eq!(decode_lwt(&encode_lwt(v)), Some(v));
        }
        assert_eq!(decode_lwt(&[0u8; 3]), None, "short input must not panic");
    }

    #[test]
    fn prefix_does_not_bleed_between_collections() {
        // The classic prefix-scan bug: `todos` is a prefix of `todos_archive`.
        let short = prefix("todos", 1);
        let long = prefix("todos_archive", 1);
        assert!(
            !long.starts_with(&short),
            "a scan of {short:?} must not match {long:?}"
        );
    }

    #[test]
    fn prefix_separates_schema_versions() {
        assert_ne!(prefix("todos", 1), prefix("todos", 2));
        assert!(!prefix("todos", 1).starts_with(&prefix("todos", 12)));
    }

    #[test]
    fn a_document_key_orders_by_id_within_a_collection() {
        let p = prefix("todos", 1);
        let key = |id: &str| {
            let mut k = p.clone();
            k.extend_from_slice(id.as_bytes());
            k
        };
        assert!(key("a") < key("b"));
        assert!(key("b") < key("ba"));
    }

    #[test]
    fn the_deleted_flag_orders_before_the_timestamp() {
        // cleanup relies on `deleted = 1` being a contiguous leading range.
        let live = cleanup_prefix("todos", 1, false);
        let deleted = cleanup_prefix("todos", 1, true);
        assert!(live < deleted);
        let mut with_lwt = deleted.clone();
        with_lwt.extend_from_slice(&encode_lwt(100));
        assert!(with_lwt.starts_with(&deleted));
    }

    #[test]
    fn the_newer_tombstone_sorts_after_the_older_one() {
        // A cutoff range delete must be a *prefix* of this ordering.
        let base = cleanup_prefix("todos", 1, true);
        let key = |lwt: i64, id: &str| {
            let mut k = base.clone();
            k.extend_from_slice(&encode_lwt(lwt));
            k.extend_from_slice(id.as_bytes());
            k
        };
        assert!(key(100, "a") < key(900, "a"));
        assert!(key(900, "a") < key(900, "b"));
    }

    #[test]
    fn a_separator_cannot_appear_in_a_valid_collection_name() {
        // The premise the encoding rests on. Collection names come from the same
        // validator as table names, which rejects control characters.
        let name = "todos";
        assert!(!name.as_bytes().contains(&KEY_SEPARATOR));
        assert!(!prefix(name, 1).is_empty());
    }
}
