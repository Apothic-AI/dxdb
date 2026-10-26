//! Applying an event-reduce action to a result array.
//!
//! Ported from `event-reduce-js` `src/actions/action-functions.ts`, function for function and
//! in the same order, so a reviewer can diff the two side by side.
//!
//! Two consequences of the upstream implementations are worth stating because they look
//! like bugs and are not:
//!
//! * **`insertAtSortPosition` refuses to insert a document that is already present.** Its own
//!   comment explains why: two documents that compare equal would make the insertion point
//!   non-deterministic. The two compound actions below depend on that: they compare the array
//!   length before and after to decide whether a removal is needed.
//! * **A delete's `document_data` is the tombstone, and a delete action removes the document
//!   by id rather than inserting anything**, so a tombstone never enters a result array. This
//!   module still refuses to insert a deleted document, belt and braces, because a mangled
//!   event should not put a tombstone in a live result.

use std::cmp::Ordering;

use dxdb_storage::{ChangeEvent, DocumentData};

use crate::event_reduce::{Action, ReduceParams};

/// The document an action inserts, or `None` for a delete or a tombstone.
///
/// Upstream reads `changeEvent.doc`, which is `null` for a delete. `ChangeEvent` in
/// `dxdb-storage` always carries `document_data` (the tombstone, for a delete), so the
/// `null` case is reconstructed here: a delete or a deleted document inserts nothing.
fn inserted_document(event: &ChangeEvent) -> Option<&DocumentData> {
    if event.operation == dxdb_storage::Operation::Delete {
        return None;
    }
    if event.document_data.is_deleted() {
        return None;
    }
    Some(&event.document_data)
}

/// The position of the changed document in the results, or `None`.
fn index_of_changed_document(
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    results: &[DocumentData],
) -> Option<usize> {
    let id = event.document_id.as_str();
    results
        .iter()
        .position(|candidate| params.document_id_matches(candidate, id))
}

/// Inserts `document` into `results` at its sort position.
///
/// Returns whether it was inserted. Mirrors `array-push-at-sort-position`'s
/// `pushAtSortPosition(list, doc, comparator, 0)`: the `0` keeps a new document *before* any
/// equal-sorting document, which is what makes the result order stable across a replay.
fn insert_at_sort_position(
    params: &ReduceParams<'_>,
    results: &mut Vec<DocumentData>,
    event: &ChangeEvent,
) -> bool {
    let Some(document) = inserted_document(event) else {
        return false;
    };
    // "If document is already in results, we cannot add it again because it would throw on
    // non-deterministic ordering."
    if index_of_changed_document(params, event, results).is_some() {
        return false;
    }
    let position = results
        .iter()
        .position(|existing| (params.sort_comparator)(document, existing) == Ordering::Less)
        .unwrap_or(results.len());
    results.insert(position, document.clone());
    true
}

/// Applies `action` to a copy of `previous_results` and returns the new array.
///
/// Only the actions that transform the array should reach here; `DoNothing`,
/// `RunFullQueryAgain` and `UnknownAction` are verdicts the caller handles.
pub fn apply_action(
    action: Action,
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    previous_results: &[DocumentData],
) -> Vec<DocumentData> {
    let mut results = previous_results.to_vec();
    match action {
        // Verdicts: no transformation. `reduce` handles these before calling in, but a
        // direct caller is not misled.
        Action::DoNothing | Action::RunFullQueryAgain | Action::UnknownAction => {}
        Action::InsertFirst => {
            if let Some(document) = inserted_document(event) {
                results.insert(0, document.clone());
            }
        }
        Action::InsertLast => {
            if let Some(document) = inserted_document(event) {
                results.push(document.clone());
            }
        }
        Action::RemoveFirstItem => {
            if !results.is_empty() {
                results.remove(0);
            }
        }
        Action::RemoveLastItem => {
            results.pop();
        }
        Action::RemoveFirstInsertLast => {
            // Upstream composes the primitives in this order; the order matters when the
            // result array is shorter than expected, so it is preserved.
            if !results.is_empty() {
                results.remove(0);
            }
            if let Some(document) = inserted_document(event) {
                results.push(document.clone());
            }
        }
        Action::RemoveLastInsertFirst => {
            results.pop();
            if let Some(document) = inserted_document(event) {
                results.insert(0, document.clone());
            }
        }
        Action::RemoveFirstInsertFirst => {
            if !results.is_empty() {
                results.remove(0);
            }
            if let Some(document) = inserted_document(event) {
                results.insert(0, document.clone());
            }
        }
        Action::RemoveLastInsertLast => {
            results.pop();
            if let Some(document) = inserted_document(event) {
                results.push(document.clone());
            }
        }
        Action::RemoveExisting => {
            if let Some(position) = index_of_changed_document(params, event, &results) {
                results.remove(position);
            }
        }
        Action::ReplaceExisting => {
            if let Some(position) = index_of_changed_document(params, event, &results) {
                match inserted_document(event) {
                    Some(document) => results[position] = document.clone(),
                    // Upstream writes `changeEvent.doc` unconditionally, which for a delete is
                    // `null`. Storing a tombstone in a live result would be worse, so a delete
                    // removes instead. Unreachable for a well-formed event: the decision table
                    // never pairs `replaceExisting` with a delete.
                    None => {
                        results.remove(position);
                    }
                }
            }
        }
        Action::AlwaysWrong => {
            // "This function always returns wrong results; it must be later optimised out."
            // It is only produced by the truth-table generator, never by the BDD, so reaching
            // it means the decision table is corrupt. Clearing the results would be the
            // upstream behaviour and is deliberately *not* reproduced: an empty result is a
            // silent wrong answer. Nothing is changed and the caller's verdict stands.
        }
        Action::InsertAtSortPosition => {
            insert_at_sort_position(params, &mut results, event);
        }
        Action::RemoveExistingAndInsertAtSortPosition => {
            if let Some(position) = index_of_changed_document(params, event, &results) {
                results.remove(position);
            }
            insert_at_sort_position(params, &mut results, event);
        }
        Action::InsertAtSortPositionRemoveLast => {
            let length_before = results.len();
            let inserted = insert_at_sort_position(params, &mut results, event);
            // Upstream compares lengths because `insertAtSortPosition` may decline; using the
            // return value is the same test without the ambiguity of a length check.
            if inserted && results.len() > length_before {
                results.pop();
            }
        }
        Action::InsertAtSortPositionRemoveFirst => {
            let length_before = results.len();
            let inserted = insert_at_sort_position(params, &mut results, event);
            if inserted && results.len() > length_before && !results.is_empty() {
                results.remove(0);
            }
        }
    }
    results
}
