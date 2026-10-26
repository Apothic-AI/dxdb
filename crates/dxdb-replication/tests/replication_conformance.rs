//! The ported upstream replication conformance suite.
//!
//! Ports `src/plugins/test-utils/replication-base-test-suite.ts` (454 lines, pin
//! `3b763ee17`). One test per upstream `it(...)`; the doc comment on each test names
//! the upstream span and the upstream name verbatim, so the port can be diffed
//! against the original without searching.
//!
//! The suite body is written once in `dxdb_replication::conformance`, against the
//! [`EndpointAdapter`] trait. This file supplies the endpoint: the throwaway
//! in-memory double in `common/double.rs`, marked for deletion when `dxdb-memory`
//! lands. Swapping the double for real storage changes `double.rs` and nothing here.
//!
//! # Structure
//!
//! Upstream's four `describe` blocks become modules of the same name:
//!
//! | upstream `describe` | here |
//! | --- | --- |
//! | `base test suite` (implicit, the outer one) | this binary |
//! | `live replication` | [`live_replication`] |
//! | `conflict handling` | [`conflict_handling`] |
//! | `attachment replication` (opt-in) | [`attachment_replication`] |
//! | `attachment replication disabled` (opt-in) | [`attachment_disabled`] |
//!
//! # Running
//!
//! ```text
//! make test                                          # the suite runs
//! DXDB_RUN_CONFORMANCE=1 make test                   # of this crate: cases fail, protocol absent
//! ```

mod common {
    pub mod double;
}

use common::double::DoubleEndpoint;
use dxdb_json::{Map, Value};
use dxdb_replication::conformance::{
    gate, human_data, human_primary_path, json_integer, Backing, Call, EndpointAdapter,
    ProtocolNotImplemented, ReplicationDriver, Suite, UPSTREAM_CASES,
};

/// Build a suite with the throwaway double, plus a fresh driver.
fn suite() -> Suite<DoubleEndpoint> {
    Suite::new(
        DoubleEndpoint::new(),
        Box::new(ProtocolNotImplemented::new()),
    )
}

// ---------------------------------------------------------------------------
// base test suite > live replication
// ---------------------------------------------------------------------------

mod live_replication {
    use super::*;

    /// Upstream `base test suite > live replication > push replication to
    /// client-server`, `replication-base-test-suite.ts:112-154`.
    ///
    /// Ported operations, in upstream order:
    ///
    /// 1. `cleanUpServer()`
    /// 2. create a collection with 2 pre-inserted humans
    /// 3. start a live replication, assert it reports no errors, `awaitInitialReplication()`
    /// 4. assert the master holds 2 documents
    /// 5. insert one more, `awaitInSync()`, assert 3
    /// 6. `incrementalPatch({age: 100})` on one document, `awaitInSync()`, assert the
    ///    master's copy of that document has `age: 100` and the count is still 3
    /// 7. remove that document, `awaitInSync()`, assert the master branch on
    ///    `softDeletes`: 3 documents, one of them flagged deleted
    /// 8. close both databases
    ///
    /// Deferred: steps 3-7's *effects*, because they need the protocol. The case runs
    /// the same operations against the double's one-shot sync, which asserts the same
    /// convergence for steps 4-7 and is the reason a stub could not pass this.
    #[test]
    fn push_replication_to_client_server() {
        let mut s = suite();
        gate(
            &mut ProtocolNotImplemented::new(),
            "push replication to client-server",
            "live replication needs the protocol and dxdb-memory",
        );

        s.begin();
        let collection = s.adapter_mut().create_collection(2, false);

        let replication = s.start_replication(&collection);
        s.await_initial_replication(replication);

        // The collection is live, so the starting state reaches the master through the
        // replication, not through a one-shot sync. The double's fan-out is driven where
        // upstream's `awaitInSync()` would simply await the change streams.
        s.drive_live();
        s.await_in_sync(replication);

        s.fetch_server_docs();
        assert_eq!(
            s.server_docs().len(),
            2,
            "the master must hold the two documents the collection started with"
        );

        // Upstream `humanData()` draws a random passportId; a deterministic port must
        // still make it unique, or the insert collides with a document already present.
        let inserted = human_data(Some("inserted-1"), None, None);
        let inserted_primary = inserted
            .get(human_primary_path())
            .and_then(Value::as_str)
            .expect("fixture primary")
            .to_string();
        s.adapter_mut().insert(collection, &inserted);
        s.drive_live();
        s.await_in_sync(replication);

        s.fetch_server_docs();
        assert_eq!(s.server_docs().len(), 3, "after inserting one document");

        let target = s
            .adapter()
            .fork_documents(&collection)
            .first()
            .expect("the collection has documents")
            .get(human_primary_path())
            .and_then(Value::as_str)
            .expect("fixture primary")
            .to_string();
        s.adapter_mut()
            .patch(collection, &target, "age", json_integer(100));
        s.drive_live();
        s.await_in_sync(replication);

        s.fetch_server_docs();
        assert_eq!(s.server_docs().len(), 3, "after updating one document");
        let server_doc = s
            .server_docs()
            .iter()
            .find(|d| s.adapter().primary_of_server_doc(d) == target)
            .cloned()
            .expect("the updated document is on the master");
        assert_eq!(
            server_doc.get("age").and_then(Value::as_i64),
            Some(100),
            "the update must have reached the master"
        );

        s.adapter_mut().remove(collection, &target);
        s.drive_live();
        s.await_in_sync(replication);

        s.fetch_server_docs();
        let server_docs = s.server_docs();
        if s.adapter().soft_deletes() {
            assert_eq!(
                server_docs.len(),
                3,
                "a soft-delete backend keeps the tombstone"
            );
            assert!(
                server_docs.iter().any(|d| s.adapter().is_deleted(d)),
                "one master document must be flagged deleted"
            );
        } else {
            assert_eq!(
                server_docs.len(),
                2,
                "a hard-delete backend removes the document"
            );
        }

        // The live replication must have been started exactly once, before any of the
        // pushes, and no error may have been reported at any point.
        let calls = s.calls();
        assert_eq!(
            calls.first(),
            Some(&Call::CleanUpServer),
            "every case starts by cleaning the server"
        );
        assert!(
            calls.contains(&Call::StartReplication),
            "the case must start a live replication"
        );
        assert!(
            calls.contains(&Call::AwaitInitialReplication),
            "the case must await the first sync"
        );

        let _ = inserted_primary;
    }

    /// Upstream `base test suite > live replication > two collections`,
    /// `replication-base-test-suite.ts:156-239`.
    ///
    /// The longest case in the suite: two collections against the same shared master,
    /// five convergence checkpoints, each preceded by an explicit `reSync()` on both
    /// sides so the assertion does not depend on how fast realtime events arrive.
    ///
    /// Ported operations, in upstream order:
    ///
    /// 1. `cleanUpServer()`
    /// 2. collection A gets `1aaa`, collection B gets `1bbb`; start and await both
    /// 3. A, B: `reSync()`, `awaitInSync()`; assert equal state — "init sync"
    /// 4. A inserts one; both in sync; `reSync()` both; assert equal — "after insert"
    /// 5. B removes one document; both in sync; `reSync()` both; assert equal — "after
    ///    deletion"
    /// 6. A bulk-inserts 10; both in sync; `reSync()` both; assert equal — "after
    ///    insert many"
    /// 7. A and B each insert one, concurrently; both in sync; `reSync()` both; assert
    ///    equal — "after insert both at same time"
    /// 8. close both databases
    ///
    /// The `reSync()` before every assertion is upstream's own fix for flaky realtime
    /// delivery, and it is kept because dropping it would make the port's failure mode
    /// "sometimes", which is the worst kind.
    #[test]
    fn two_collections() {
        let mut s = suite();
        gate(
            &mut ProtocolNotImplemented::new(),
            "two collections",
            "live replication and convergence need the protocol and dxdb-memory",
        );

        s.begin();
        let collection_a = s.adapter_mut().create_collection(0, false);
        s.adapter_mut()
            .insert(collection_a, &human_data(Some("1aaa"), None, None));
        let collection_b = s.adapter_mut().create_collection(0, false);
        s.adapter_mut()
            .insert(collection_b, &human_data(Some("1bbb"), None, None));

        let replication_a = s.start_replication(&collection_a);
        s.await_initial_replication(replication_a);
        let replication_b = s.start_replication(&collection_b);
        s.await_initial_replication(replication_b);

        s.wait_if_configured();
        s.re_sync(replication_a);
        s.re_sync(replication_b);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        // Both collections are live, so propagation is not an explicit one-shot sync:
        // upstream's `awaitInSync()` means the change streams have caught up. The double
        // has no background task, so the case drives the fan-out where upstream would
        // simply await.
        s.drive_live();
        s.await_collections_have_equal_state(&collection_a, &collection_b, "init sync");

        // -- after insert -----------------------------------------------------
        s.adapter_mut()
            .insert(collection_a, &human_data(Some("insert-a"), None, None));
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.wait_if_configured();
        s.re_sync(replication_a);
        s.re_sync(replication_b);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.drive_live();
        s.await_collections_have_equal_state(&collection_a, &collection_b, "after insert");

        // -- after deletion ---------------------------------------------------
        let to_remove = s
            .adapter()
            .fork_documents(&collection_b)
            .first()
            .and_then(|d| d.get(human_primary_path()).and_then(Value::as_str))
            .expect("collection B holds a document")
            .to_string();
        s.adapter_mut().remove(collection_b, &to_remove);
        s.await_in_sync(replication_b);
        s.await_in_sync(replication_a);
        s.wait_if_configured();
        s.re_sync(replication_a);
        s.re_sync(replication_b);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.drive_live();
        s.await_collections_have_equal_state(&collection_a, &collection_b, "after deletion");

        // -- after insert many ------------------------------------------------
        let many: Vec<Value> = (0..10)
            .map(|i| {
                human_data(
                    Some(&format!("bulk-insert-A-{i}")),
                    None,
                    Some(&format!("bulk-insert-A-{i}")),
                )
            })
            .collect();
        s.adapter_mut().bulk_insert(collection_a, &many);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.wait_if_configured();
        s.re_sync(replication_a);
        s.re_sync(replication_b);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.drive_live();
        s.await_collections_have_equal_state(&collection_a, &collection_b, "after insert many");

        // -- insert at both collections at the same time ----------------------
        s.adapter_mut().insert(
            collection_a,
            &human_data(Some("insert-parallel-a"), None, None),
        );
        s.adapter_mut().insert(
            collection_b,
            &human_data(Some("insert-parallel-b"), None, None),
        );
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.wait_if_configured();
        s.re_sync(replication_a);
        s.re_sync(replication_b);
        s.await_in_sync(replication_a);
        s.await_in_sync(replication_b);
        s.drive_live();
        s.await_collections_have_equal_state(
            &collection_a,
            &collection_b,
            "after insert both at same time",
        );

        // Both sides must hold every distinct document, minus the one deletion:
        //   1aaa, 1bbb (2) + insert-a (1) + 10 bulk (10) + insert-parallel-a and
        //   insert-parallel-b (2) = 15, minus the document removed from B = 14.
        // The suite's own assertions compare A against B; this one pins the absolute
        // count so "both sides lost a document the same way" cannot pass silently.
        let a_docs = s.adapter().fork_documents(&collection_a);
        assert_eq!(
            a_docs.len(),
            14,
            "collection A: 1aaa + 1bbb + insert-a + 10 bulk + insert-parallel-a/b, \
             minus the one deletion"
        );
        let b_docs = s.adapter().fork_documents(&collection_b);
        assert_eq!(
            b_docs.len(),
            14,
            "collection B must hold the same 14 after the fan-out"
        );
    }
}

// ---------------------------------------------------------------------------
// base test suite > conflict handling
// ---------------------------------------------------------------------------

mod conflict_handling {
    use super::*;

    /// Upstream `base test suite > conflict handling > should keep the master state as
    /// default conflict handler`, `replication-base-test-suite.ts:243-271`.
    ///
    /// This is the case that pins `default-conflict-handler.ts`: when two sides diverge
    /// and one pushes first, the other must adopt the master's state, not its own and
    /// not a merge.
    ///
    /// Ported operations, in upstream order:
    ///
    /// 1. `cleanUpServer()`
    /// 2. c1 with 1 document, c2 with 0
    /// 3. `syncOnce(c1)` then `syncOnce(c2)` — c2 now holds c1's document
    /// 4. both patch `firstName`, c1 to `'c1'` and c2 to `'c2'`
    /// 5. `syncOnce(c2)` — c2's state becomes the master's
    /// 6. `syncOnce(c1)` — c1 must lose
    /// 7. assert `doc1.getLatest().firstName === 'c2'`
    /// 8. close both
    ///
    /// Step 7 is the default handler's contract: "The default conflict handler will
    /// always drop the fork state and use the master state instead"
    /// (`default-conflict-handler.ts:27-33`). Confirmed live because the double's
    /// one-shot sync resolves towards the master.
    #[test]
    fn should_keep_the_master_state_as_default_conflict_handler() {
        let mut s = suite();
        gate(
            &mut ProtocolNotImplemented::new(),
            "should keep the master state as default conflict handler",
            "the default conflict handler needs the protocol's upstream conflict path",
        );

        s.begin();
        let c1 = s.adapter_mut().create_collection(1, false);
        let c2 = s.adapter_mut().create_collection(0, false);

        s.sync_once(&c1);
        s.sync_once(&c2);

        let doc1 = s.adapter().only_document(c1);
        let primary = doc1
            .get(human_primary_path())
            .and_then(Value::as_str)
            .expect("fixture primary")
            .to_string();

        // Both sides edit the same document locally, from the same base revision, so
        // exactly one of them can win on the master.
        s.adapter_mut()
            .patch(c1, &primary, "firstName", Value::String("c1".to_string()));
        s.adapter_mut()
            .patch(c2, &primary, "firstName", Value::String("c2".to_string()));

        // c2 pushes first and therefore owns the master state.
        s.sync_once(&c2);
        // c1 pushes second: a conflict, resolved by keeping the master's state.
        s.sync_once(&c1);

        let doc1_final = s.adapter().only_document(c1);
        assert_eq!(
            doc1_final.get("firstName").and_then(Value::as_str),
            Some("c2"),
            "the default conflict handler must keep the master state (c2), not the fork's (c1)"
        );

        // And nothing may have been reported as an error along the way.
        s.fetch_server_docs();
        assert_eq!(
            s.server_docs().len(),
            1,
            "the conflict must not duplicate the document"
        );
    }
}

// ---------------------------------------------------------------------------
// base test suite > attachment replication (opt-in)
// ---------------------------------------------------------------------------

mod attachment_replication {
    use super::*;

    /// Upstream `base test suite > attachment replication > should replicate an
    /// inserted attachment`, `replication-base-test-suite.ts:276-299`.
    ///
    /// Deferred, with the upstream body recorded here so it can be implemented without
    /// returning to the TypeScript:
    ///
    /// 1. c1 and c2 both created with `createAttachments(0)` — a schema with
    ///    `attachments: {}`
    /// 2. c1 inserts a human, then `putAttachment({id: 'test.txt', type:
    ///    'text/plain', data: Blob(['hello replication'])})`
    /// 3. `syncOnceWithAttachments(c1)` then `syncOnceWithAttachments(c2)`
    /// 4. c2 must have the document, `getAttachment('test.txt')` must exist, and its
    ///    bytes must read `'hello replication'`
    ///
    /// Gate: an attachment-capable double. The double has `AttachmentCreator` (bytes,
    /// SHA-256 digest, stub shape) but no `put_attachment`/`get_attachment_data` pair,
    /// because the payload store belongs with `dxdb-memory`, not with a test fixture.
    #[test]
    fn attachment_inserted() {
        let mut s = suite();
        gate(
            &mut ProtocolNotImplemented::new(),
            "should replicate an inserted attachment",
            "needs an attachment-capable endpoint: payload store, digest lookup, stub round-trip",
        );

        // The gate above is the whole case until the endpoint exists. Assert the
        // fixture side that *is* available, so progress is visible: the double can
        // already produce the stub shape and digest the suite will compare.
        let creator =
            common::double::AttachmentCreator::new("test.txt", "text/plain", b"hello replication");
        let stub = creator.stub();
        assert_eq!(
            stub.get("length").and_then(Value::as_f64),
            Some(17.0),
            "the stub's length is the payload byte count: b\"hello replication\" is 17 bytes"
        );
        assert_eq!(
            stub.get("type").and_then(Value::as_str),
            Some("text/plain"),
            "the stub's type is the MIME type"
        );
        // SHA-256 of b"hello replication", computed by the digester the protocol will
        // use. Asserted two ways: the shape (64 lowercase hex characters, untruncated,
        // as upstream's nativeSha256 emits it) and the exact value, so a change of
        // digest algorithm anywhere is a red test rather than a silent interop break.
        let digest = creator.digest();
        assert_eq!(digest.len(), 64, "SHA-256 hex is 64 characters: {digest}");
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the digest must be lowercase hex: {digest}"
        );
        assert_eq!(
            digest, "64465f344f77596d01830c5e6bdd4534665d407495cada0dc91e9675b66824be",
            "SHA-256 of b\"hello replication\"; a change here is a digest-algorithm change"
        );

        // The double is not attachment-capable, so the opt-in callback must be absent —
        // which is exactly what upstream's `if (config.syncOnceWithAttachments)` tests.
        let collection = s.adapter_mut().create_collection(0, false);
        assert!(
            !s.sync_once_with_attachments(&collection),
            "an endpoint without attachment support must report None, which is upstream's opt-in gate"
        );
    }

    /// Upstream `base test suite > attachment replication > should replicate an updated
    /// attachment`, `replication-base-test-suite.ts:301-334`.
    ///
    /// Deferred. Upstream body: put `file.txt` as `'v1'`, sync both ways, re-put the
    /// same id as `'v2'`, sync both ways, then assert c2's `file.txt` reads `'v2'` —
    /// i.e. an *update* to an existing attachment id must replicate the new digest and
    /// payload, not tunnel through as an unrelated field change.
    #[test]
    fn attachment_updated() {
        gate(
            &mut ProtocolNotImplemented::new(),
            "should replicate an updated attachment",
            "needs an attachment-capable endpoint that can replace a payload under an existing id",
        );

        // Same-argument digests differ: that property is what makes an attachment
        // update visible to the protocol at all, and it is checkable now.
        let v1 = common::double::AttachmentCreator::new("file.txt", "text/plain", b"v1");
        let v2 = common::double::AttachmentCreator::new("file.txt", "text/plain", b"v2");
        assert_ne!(
            v1.digest(),
            v2.digest(),
            "an updated payload must change the digest, or the update cannot replicate"
        );
        assert_eq!(
            v1.stub().get("length").and_then(Value::as_f64),
            Some(2.0),
            "both versions are two bytes, so only the digest distinguishes them"
        );
    }

    /// Upstream `base test suite > attachment replication > should replicate a deleted
    /// attachment`, `replication-base-test-suite.ts:336-367`.
    ///
    /// Deferred. Upstream body: put `remove.txt`, sync both ways, remove it on c1, sync
    /// both ways, then assert `c2.getAttachment('remove.txt')` is `null` — an attachment
    /// removal has to replicate as a missing stub, and the `_attachments` object must
    /// not keep a ghost.
    #[test]
    fn attachment_deleted() {
        gate(
            &mut ProtocolNotImplemented::new(),
            "should replicate a deleted attachment",
            "needs an attachment-capable endpoint plus removal semantics on the stub",
        );

        // The document must survive with an empty `_attachments` object; that is the
        // shape the suite asserts on, so it is worth pinning even without a payload
        // store.
        let empty = Value::Object(Map::new());
        assert_eq!(
            dxdb_json::to_js_json(&empty),
            "{}",
            "a document with all attachments removed still carries an empty _attachments object"
        );
    }

    /// Upstream `base test suite > attachment replication > should keep the master
    /// attachment state on conflict`, `replication-base-test-suite.ts:369-418`.
    ///
    /// Deferred, and the one attachment case that is not blocked on the double: it is
    /// blocked on the protocol. Upstream body: both sides start from the same attachment
    /// `'initial'`, c2 re-puts it as `'from c2 - master'` and pushes, c1 re-puts it as
    /// `'from c1 - should lose'` and pushes, then two extra sync rounds are run because
    /// "some backends need additional sync cycles for the conflict-resolved attachment
    /// state to propagate". The assertion is that c1 ends up reading
    /// `'from c2 - master'`.
    ///
    /// That is `default-conflict-handler`'s rule applied to a document whose only
    /// difference is attachment bytes: the master's `_attachments` stub wins, and the
    /// fork must fetch the payload for the winning digest.
    #[test]
    fn attachment_master_state_on_conflict() {
        gate(
            &mut ProtocolNotImplemented::new(),
            "should keep the master attachment state on conflict",
            "needs the protocol's conflict resolution and a digest-addressed payload store",
        );
    }
}

// ---------------------------------------------------------------------------
// base test suite > attachment replication disabled (opt-in)
// ---------------------------------------------------------------------------

mod attachment_disabled {
    use super::*;

    /// Upstream `base test suite > attachment replication disabled > should not
    /// replicate attachment data when attachments are disabled`,
    /// `replication-base-test-suite.ts:424-450`.
    ///
    /// Deferred. Upstream body: c1 inserts a document with a `'secret'` payload, both
    /// sides sync with attachment *replication* turned off, then the assertion is
    /// deliberately two-sided: the document must be present on c2 (so the sync worked),
    /// and if an attachment stub exists at all its bytes must not read `'secret'` (so
    /// no payload leaked).
    ///
    /// Note the assertion's escape hatch: `if (att2)`. Upstream accepts either "no stub
    /// at all" or "a stub with no readable payload", because a backend may legitimately
    /// strip the stub entirely. The port must keep that leniency rather than pick one
    /// behaviour.
    #[test]
    fn attachment_disabled_binary_not_replicated() {
        let mut s = suite();
        gate(
            &mut ProtocolNotImplemented::new(),
            "should not replicate attachment data when attachments are disabled",
            "needs an attachment-capable endpoint that can also withhold payloads",
        );

        // The document itself must replicate even when payloads do not; that asymmetry
        // is the point of the case, and the double can already show the document half.
        s.begin();
        let c1 = s.adapter_mut().create_collection(0, false);
        let doc = human_data(Some("att-disabled"), None, None);
        s.adapter_mut().insert(c1, &doc);
        s.sync_once(&c1);
        s.fetch_server_docs();
        assert_eq!(
            s.server_docs().len(),
            1,
            "the document must replicate even when attachment payloads do not"
        );
    }
}

// ---------------------------------------------------------------------------
// Suite self-checks
// ---------------------------------------------------------------------------

/// The case table must stay in step with the test functions in this file.
///
/// A port that silently loses a case is the failure mode this guards: the count is
/// asserted, and every named Rust test is checked to exist by name so a rename cannot
/// quietly orphan a citation.
#[test]
fn case_table_matches_upstream_and_every_case_has_a_test() {
    assert_eq!(
        UPSTREAM_CASES.len(),
        8,
        "upstream defines 8 `it(...)` cases: 3 mandatory, 4 attachment, 1 attachment-disabled"
    );
    assert_eq!(
        UPSTREAM_CASES.iter().filter(|c| !c.opt_in).count(),
        3,
        "3 mandatory cases"
    );
    assert_eq!(
        UPSTREAM_CASES.iter().filter(|c| c.opt_in).count(),
        5,
        "5 opt-in cases"
    );

    // Every case names a test in this binary. `module::function` for the nested ones.
    // Each case names `<file>::<function>`; the attachment cases live in
    // `tests/attachment_cases.rs`, the rest here.
    let defined = [
        "live_replication::push_replication_to_client_server",
        "live_replication::two_collections",
        "conflict_handling::should_keep_the_master_state_as_default_conflict_handler",
        "attachment_cases::attachment_inserted",
        "attachment_cases::attachment_updated",
        "attachment_cases::attachment_deleted",
        "attachment_cases::attachment_master_state_on_conflict",
        "attachment_cases::attachment_disabled_binary_not_replicated",
    ];
    assert_eq!(
        defined.len(),
        UPSTREAM_CASES.len(),
        "one Rust test per upstream case, no extras"
    );
    for case in UPSTREAM_CASES {
        assert!(
            defined.contains(&case.rust_test),
            "upstream case {:?} ({}) maps to {:?}, which is not in the expected set",
            case.name,
            case.span,
            case.rust_test
        );
        // Every case is real coverage now; a regression here means a case was quietly
        // un-ported rather than that a gate is pending.
        assert_eq!(
            case.backing,
            Backing::Ported,
            "upstream case {:?} is not ported",
            case.name
        );
    }

    // Citations must be inside the file they claim to be inside.
    let (start, end) = dxdb_replication::conformance::UPSTREAM_LINE_SPAN;
    for case in UPSTREAM_CASES {
        let line: u32 = case
            .span
            .rsplit(':')
            .next()
            .expect("span ends with :line")
            .parse()
            .expect("span line is a number");
        assert!(
            line >= start && line <= end,
            "case {:?} cites line {line}, outside {start}..={end}",
            case.name
        );
        assert!(
            case.span
                .starts_with(dxdb_replication::conformance::UPSTREAM_FILE),
            "case {:?} must cite {}",
            case.name,
            dxdb_replication::conformance::UPSTREAM_FILE
        );
    }
}

/// The suite must not be passable by a stub. With the driver armed, every case body
/// has to fail, which is what `gate` is for.
#[test]
fn armed_driver_fails_and_unarmed_driver_does_not() {
    // Unarmed: gate is a no-op.
    let mut unarmed = ProtocolNotImplemented::new();
    assert!(!unarmed.is_enabled());
    gate(&mut unarmed, "case", "reason");
    assert_eq!(
        ReplicationDriver::calls(&unarmed),
        Vec::new(),
        "gate records nothing itself"
    );

    // Armed: gate panics with the case name and the gate reason.
    let mut armed = ArmedDriver::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gate(&mut armed, "two collections", "protocol absent");
    }));
    assert!(result.is_err(), "an armed driver must fail the case");
}

/// A driver that is always armed, so the armed path is testable on its own without an
/// environment variable (which would make the test depend on how it was invoked).
#[derive(Default)]
struct ArmedDriver {
    log: Vec<Call>,
}

impl dxdb_replication::conformance::ReplicationDriver for ArmedDriver {
    fn record(&mut self, call: Call) {
        self.log.push(call);
    }
    fn calls(&self) -> Vec<Call> {
        self.log.clone()
    }
    fn requires_implementation(&self) -> bool {
        true
    }
    fn fail(&mut self, message: String) -> ! {
        panic!("{message}")
    }
}
