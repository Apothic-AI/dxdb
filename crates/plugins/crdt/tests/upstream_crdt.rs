//! Ported from `test/unit/crdt.test.ts` (665 lines) at upstream `3b763ee17`.
//!
//! Each test names the upstream `it(...)` and its line. The cases about the operation log,
//! the if/else branches, the merge and the conflict handler's `isEqual`/`resolve` are
//! ported; the ones that need a live collection and the eight intercepted document methods
//! are listed in the crate docs as not ported.
//!
//! Note that upstream's `.updateCRDT()` describe block is preceded by a bare `return;`
//! (`:329`, "TODO"), so **those two cases do not run upstream either**. They are ported here
//! anyway — the behaviour is in the source and is the core of the plugin — and the fact that
//! upstream's own suite skips them is recorded rather than passed over, because it means a
//! regression there would not have been caught upstream.
//!
//! The merge tests (`:617` "should merge the +1 increments") are the heart of it: two
//! replicas that each incremented must both land, which is the property that makes a CRDT
//! worth having over last-write-wins.

use dxdb_plugin_crdt::{
    append_operation, check_incremental_modify, check_modify, compare_operations, crdt_schema_part,
    crdt_states_equal, merge_crdt_fields, read_crdt_field, rebuild_from_crdt, redirect_patch,
    redirect_remove, redirect_update, resolve_crdt_conflict, run_operation_on_document,
    seeded_document, CrdtDocumentField, CrdtEntry, CrdtOperation, Error, CRDT_CONTEXT,
};
use serde_json::{json, Value};

const FIELD: &str = "crdts";

/// `getCRDTCollection`'s human schema uses `crdts` as the field.
fn always(_document: &Value, _selector: &Value) -> bool {
    true
}

fn by_passport(document: &Value, selector: &Value) -> bool {
    // The only selector the tests use is `passportId: {$exists: true}`, so a direct check is
    // enough and keeps the test independent of dxdb-query.
    if let Some(conditions) = selector.as_object() {
        return conditions.iter().all(|(field, condition)| {
            let present = document.get(field).is_some();
            match condition.get("$exists").and_then(Value::as_bool) {
                Some(want) => present == want,
                None => document.get(field) == Some(condition),
            }
        });
    }
    false
}

/// A field with one operation, as `insert` produces (`index.ts:99-115`).
fn seeded(creator: &str, entries: Vec<CrdtEntry>, time: f64) -> CrdtDocumentField {
    let mut field = CrdtDocumentField::new();
    field
        .operations
        .push(vec![CrdtOperation::new(entries, creator, time)]);
    field.recompute_hash();
    field
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:109` — should insert a document and initialize the crdt state
// ---------------------------------------------------------------------------

#[test]
fn upstream_109_an_insert_seeds_the_crdt_field() {
    // `:109-121`. The document's visible fields come from the operation, and the CRDT field
    // holds exactly one operation in one inner array.
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "foo", "age": 1 } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");

    assert_eq!(document["passportId"], json!("foo"));
    assert_eq!(document["age"], json!(1));
    assert_eq!(document["_deleted"], json!(false));
    let crdts = read_crdt_field(&document, FIELD).expect("field");
    assert_eq!(crdts.operations.len(), 1, "one write, one inner array");
    assert_eq!(crdts.operations[0].len(), 1);
    assert!(!crdts.hash.is_empty());
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:138` — should insert the document
// ---------------------------------------------------------------------------

#[test]
fn upstream_138_a_second_insert_crdt_updates_the_existing_document() {
    // `:138-159`: the second `insertCRDT` on the same primary key conflicts, is caught, and
    // becomes an update (`index.ts:116-127`). The observable result is that the second
    // write's value wins and a second operation exists.
    let first = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "firstName": "alice" } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("first");

    // The conflict path: the caller catches the insert conflict and appends instead.
    let second = append_operation(
        &first,
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "firstName": "foobar" } }),
        )],
        "creator-a",
        2.0,
        always,
    )
    .expect("second");

    assert_eq!(second["firstName"], json!("foobar"));
    let crdts = read_crdt_field(&second, FIELD).expect("field");
    assert_eq!(crdts.operations.len(), 2, "two writes");
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:192` — should respect the if-else logic
// ---------------------------------------------------------------------------

#[test]
fn upstream_192_the_if_else_branches_are_selector_driven() {
    // `:192-216`. A document with `passportId` present takes `ifMatch` (incrementing age);
    // one without takes `ifNotMatch` (setting the whole doc).
    let entry = CrdtEntry::conditional(
        json!({ "passportId": { "$exists": true } }),
        Some(json!({ "$inc": { "age": 1 } })),
        Some(json!({ "$set": { "passportId": "foo", "age": 1 } })),
    );

    let matching = seeded_document(FIELD, vec![entry.clone()], "creator-a", 1.0, by_passport)
        .expect("matched");
    // Nothing to match yet, so this is the `ifNotMatch` branch: the document is created.
    assert_eq!(matching["passportId"], json!("foo"));
    assert_eq!(matching["age"], json!(1));

    // Now apply the same entry to a document that already has the field: `ifMatch` runs.
    let incremented =
        append_operation(&matching, FIELD, vec![entry], "creator-a", 2.0, by_passport)
            .expect("incremented");
    assert_eq!(
        incremented["age"],
        json!(2),
        "the ifMatch branch incremented"
    );
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:219` — should delete the document via .remove
// ---------------------------------------------------------------------------

#[test]
fn upstream_219_remove_becomes_a_crdt_operation() {
    // `:219-233`, and the redirect at `index.ts:341-360`. The assertion upstream makes is on
    // the second operation's body: `ifMatch.$set._deleted === true`.
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 1 } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");

    let removed = append_operation(
        &document,
        FIELD,
        vec![redirect_remove()],
        "creator-a",
        2.0,
        always,
    )
    .expect("remove");

    assert_eq!(removed["_deleted"], json!(true));
    let crdts = read_crdt_field(&removed, FIELD).expect("field");
    let second = &crdts.operations[1][0];
    assert_eq!(
        second.body[0].if_match,
        Some(json!({ "$set": { "_deleted": true } })),
        "the tombstone is recorded in the log, not applied directly"
    );
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:236` — should update the document (.incrementalPatch)
// ---------------------------------------------------------------------------

#[test]
fn upstream_236_patch_is_redirected_through_update_crdt() {
    // `:236-259` and `:288-301`. `patch` becomes `updateCRDT({ifMatch: {$set: patch}})`.
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 1 } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");

    let patched = append_operation(
        &document,
        FIELD,
        vec![redirect_patch(&json!({ "age": 5 }))],
        "creator-a",
        2.0,
        always,
    )
    .expect("patch");
    assert_eq!(patched["age"], json!(5));
    assert_eq!(
        redirect_patch(&json!({ "age": 5 })),
        CrdtEntry::if_match(json!({ "$set": { "age": 5 } }))
    );
}

#[test]
fn upstream_312_update_is_redirected_through_update_crdt() {
    // `:312-327`. `update` is passed through as the `ifMatch` update itself, not wrapped in
    // `$set` — because `update` already takes a modifier document.
    assert_eq!(
        redirect_update(&json!({ "$inc": { "age": 1 } })),
        CrdtEntry::if_match(json!({ "$inc": { "age": 1 } }))
    );
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:261` and `:273` — disallowed methods
// ---------------------------------------------------------------------------

#[test]
fn upstream_261_incremental_modify_is_refused() {
    // `:261-272` expects `CRDT2`. The plugin's own write uses the `rx-crdt` context.
    assert_eq!(
        check_incremental_modify("anything-else").expect_err("refused"),
        Error::DirectIncrementalModify("anything-else".to_string())
    );
    check_incremental_modify(CRDT_CONTEXT).expect("the plugin's own write goes through");
}

#[test]
fn upstream_273_modify_is_refused() {
    // `:273-286` expects `CRDT4`. Always refused: a `modify` would change a visible field
    // without recording an operation, so the next merge would silently undo it.
    assert_eq!(check_modify().expect_err("refused"), Error::DirectModify);
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:330` — .updateCRDT() (upstream has a `return` before these: they do NOT run)
// ---------------------------------------------------------------------------

#[test]
fn upstream_330_update_crdt_appends_and_applies() {
    // `:330-350`, skipped upstream by the bare `return` at `:329`. Ported anyway: the
    // behaviour is in the source and is the core of the plugin.
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 1 } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");

    let updated = append_operation(
        &document,
        FIELD,
        vec![CrdtEntry::if_match(json!({ "$inc": { "age": 1 } }))],
        "creator-a",
        2.0,
        always,
    )
    .expect("update");

    assert_eq!(updated["age"], json!(2));
    let crdts = read_crdt_field(&updated, FIELD).expect("field");
    assert_eq!(
        crdts.operations[1][0].body[0].if_match,
        Some(json!({ "$inc": { "age": 1 } })),
        "the operation records the increment"
    );
}

#[test]
fn upstream_351_update_crdt_can_delete() {
    // `:351-371`, also skipped upstream.
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p" } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");
    let deleted = append_operation(
        &document,
        FIELD,
        vec![CrdtEntry::if_match(json!({ "$set": { "_deleted": true } }))],
        "creator-a",
        2.0,
        always,
    )
    .expect("delete");
    assert_eq!(deleted["_deleted"], json!(true));
}

// ---------------------------------------------------------------------------
// `crdt.test.ts:390` and `:500` — .getCRDTConflictHandler()
// ---------------------------------------------------------------------------

#[test]
fn upstream_390_two_inserts_merge_into_one_slot_with_two_operations() {
    // `:390-…` asserts `crdtData.operations[0].length === 2` after resolving two concurrent
    // inserts of the same document. Each replica wrote one op into slot 0; the merge unions
    // them.
    let replica_a = seeded(
        "creator-a",
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 1 } }),
        )],
        1.0,
    );
    let replica_b = seeded(
        "creator-b",
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 2 } }),
        )],
        2.0,
    );

    let merged = merge_crdt_fields(&replica_a, &replica_b);
    assert_eq!(merged.operations.len(), 1, "one slot");
    assert_eq!(merged.operations[0].len(), 2, "both replicas' ops survive");
    // And they are sorted by creator, so the merge is deterministic.
    assert_eq!(merged.operations[0][0].creator, "creator-a");
    assert_eq!(merged.operations[0][1].creator, "creator-b");
}

#[test]
fn upstream_390_merging_is_commutative() {
    // The property the conflict handler depends on: merge(a, b) == merge(b, a).
    let replica_a = seeded(
        "aaa",
        vec![CrdtEntry::if_match(json!({ "$set": { "x": 1 } }))],
        1.0,
    );
    let replica_b = seeded(
        "zzz",
        vec![CrdtEntry::if_match(json!({ "$set": { "x": 2 } }))],
        2.0,
    );
    assert_eq!(
        merge_crdt_fields(&replica_a, &replica_b),
        merge_crdt_fields(&replica_b, &replica_a)
    );
}

#[test]
fn upstream_390_is_equal_compares_hashes() {
    // `:396-408`: the same document against itself is equal.
    let field = seeded(
        "creator-a",
        vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
        1.0,
    );
    assert!(crdt_states_equal(&field, &field.clone()));

    let other = seeded(
        "creator-b",
        vec![CrdtEntry::if_match(json!({ "$set": { "a": 1 } }))],
        1.0,
    );
    assert!(
        !crdt_states_equal(&field, &other),
        "a different creator differs"
    );
}

#[test]
fn upstream_617_concurrent_increments_both_land() {
    // THE POINT OF THE PLUGIN. `:617-…` "should merge the +1 increments": two replicas each
    // `$inc` the same field, and after the merge both increments are present.
    let base = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p", "age": 0 } }),
        )],
        "creator-base",
        1.0,
        always,
    )
    .expect("base");

    // Two replicas each apply one increment.
    let replica_a = append_operation(
        &base,
        FIELD,
        vec![CrdtEntry::if_match(json!({ "$inc": { "age": 1 } }))],
        "creator-a",
        2.0,
        always,
    )
    .expect("a");
    let replica_b = append_operation(
        &base,
        FIELD,
        vec![CrdtEntry::if_match(json!({ "$inc": { "age": 1 } }))],
        "creator-b",
        3.0,
        always,
    )
    .expect("b");

    // Resolve the conflict: merge then rebuild.
    let resolved = resolve_crdt_conflict(FIELD, &replica_a, &replica_b, always).expect("resolve");
    assert_eq!(
        resolved["age"],
        json!(2),
        "both increments landed — last-write-wins would have produced 1"
    );
}

#[test]
fn a_rebuild_reproduces_the_visible_fields_from_the_log_alone() {
    // `rebuildFromCRDT` (`:279-295`): the document is a fold, so a rebuild from the CRDT field
    // must equal the document it came from.
    let document = seeded_document(
        FIELD,
        vec![
            CrdtEntry::if_match(json!({ "$set": { "passportId": "p", "age": 1 } })),
            CrdtEntry::if_match(json!({ "$set": { "name": "alice" } })),
        ],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");
    let crdts = read_crdt_field(&document, FIELD).expect("field");
    let rebuilt = rebuild_from_crdt(&document, FIELD, &crdts, always).expect("rebuild");

    for field in ["passportId", "age", "name", "_deleted"] {
        assert_eq!(
            rebuilt.get(field),
            document.get(field),
            "{field} must be derivable from the log"
        );
    }
}

#[test]
fn a_rebuild_does_not_carry_stale_visible_fields() {
    // The base is `{_deleted: false}` plus the log, so a field that the log never sets must
    // not appear even if the stored document has it. That is what stops a merge from
    // resurrecting a value a concurrent write removed.
    let mut document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p" } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");
    document["stale"] = json!("from a direct write");
    let crdts = read_crdt_field(&document, FIELD).expect("field");
    let rebuilt = rebuild_from_crdt(&document, FIELD, &crdts, always).expect("rebuild");
    assert!(
        rebuilt.get("stale").is_none(),
        "a field outside the log is not part of the derived document"
    );
}

// ---------------------------------------------------------------------------
// The log's shape and its validation
// ---------------------------------------------------------------------------

#[test]
fn an_operation_with_an_empty_body_is_rejected() {
    // The schema's `minItems: 1` on `body` (`:191`).
    assert!(matches!(
        CrdtOperation::from_json(&json!({ "body": [], "creator": "a", "time": 1 })),
        Err(Error::Malformed(_))
    ));
}

#[test]
fn the_schema_part_is_a_legal_schema_fragment() {
    // Composed the way a caller would, and validated by the schema layer.
    let part = crdt_schema_part();
    let schema = json!({
        "title": "crdt-human",
        "version": 0,
        "type": "object",
        "primaryKey": "passportId",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "age": { "type": "integer" },
            "crdts": part,
        },
        "required": ["passportId", "crdts"],
    });
    let filled = dxdb_schema::fill_with_default_settings(&schema).expect("fill");
    dxdb_schema::create_rx_schema(&filled).expect("the crdt schema fragment must compose");
}

#[test]
fn a_document_validates_against_the_crdt_schema() {
    let part = crdt_schema_part();
    let schema = dxdb_schema::fill_with_default_settings(&json!({
        "title": "crdt-human",
        "version": 0,
        "type": "object",
        "primaryKey": "passportId",
        "properties": {
            "passportId": { "type": "string", "maxLength": 100 },
            "crdts": part,
        },
        "required": ["passportId", "crdts"],
    }))
    .expect("fill");

    let mut document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p" } }),
        )],
        "creator-a",
        1.0,
        always,
    )
    .expect("seed");
    let map = document.as_object_mut().expect("object");
    map.insert("_deleted".to_string(), json!(false));
    map.insert("_rev".to_string(), json!("1-tok"));
    map.insert("_meta".to_string(), json!({ "lwt": 1 }));
    map.insert("_attachments".to_string(), json!({}));

    let errors = dxdb_schema::validate_document(&schema, &document).expect("validation");
    assert!(errors.is_empty(), "crdt document is invalid: {errors:?}");
}

#[test]
fn a_missing_crdt_field_is_reported_by_name() {
    let error = read_crdt_field(&json!({ "passportId": "p" }), FIELD).expect_err("missing");
    assert_eq!(error, Error::MissingCrdtField(FIELD.to_string()));
}

#[test]
fn append_needs_at_least_one_entry() {
    let document = seeded_document(
        FIELD,
        vec![CrdtEntry::if_match(
            json!({ "$set": { "passportId": "p" } }),
        )],
        "a",
        1.0,
        always,
    )
    .expect("seed");
    assert!(append_operation(&document, FIELD, Vec::new(), "a", 2.0, always).is_err());
}

#[test]
fn the_operations_sort_comparator_is_creator_then_equal() {
    let a = CrdtOperation::new(
        vec![CrdtEntry::if_match(json!({ "$set": { "x": 1 } }))],
        "a",
        1.0,
    );
    let b = CrdtOperation::new(
        vec![CrdtEntry::if_match(json!({ "$set": { "x": 2 } }))],
        "b",
        1.0,
    );
    assert_eq!(compare_operations(&a, &b), std::cmp::Ordering::Less);
    assert_eq!(compare_operations(&a, &a), std::cmp::Ordering::Equal);
}

#[test]
fn run_operation_leaves_the_document_alone_when_no_branch_applies() {
    // An entry with no `ifMatch` and a non-matching selector has nothing to do.
    let entry = CrdtEntry::conditional(json!({ "missing": { "$exists": true } }), None, None);
    let operation = CrdtOperation::new(vec![entry], "a", 1.0);
    let result =
        run_operation_on_document(&json!({ "x": 1 }), &operation, by_passport).expect("run");
    assert_eq!(result, json!({ "x": 1 }));
}
