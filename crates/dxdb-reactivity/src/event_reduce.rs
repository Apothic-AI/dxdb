//! Event reduce: decide whether a live query can be updated from a change event.
//!
//! Ported from upstream `src/event-reduce.ts` (292 lines) and the algorithm it delegates
//! to, `event-reduce-js` 7.4.0 (`src/states/state-resolver.ts`, `src/actions/`, and the
//! packed BDD in `src/bdd/bdd.generated.ts`).
//!
//! # Why the conservatism direction matters more than the accuracy
//!
//! A verdict of "the event is sufficient, update in place" that is wrong means a live query
//! **silently misses a change** — the user sees stale data and nothing errors. A verdict of
//! "re-run the full query" that is unnecessary costs one query. So every uncertain case
//! must fall towards `run_full_query_again`, and this port does that in two places:
//!
//! * any action this module does not implement exactly becomes `run_full_query_again`;
//! * any state combination outside the recorded table becomes `run_full_query_again`.
//!
//! The table in `testdata/event-reduce-action-table.json` is the complete decision surface
//! of the real library: all 2^18 combinations of the 18 states, recorded by driving the
//! library's own packed BDD. A port is correct iff it agrees on every row, which is a total
//! equivalence target rather than a sample set.
//!
//! # The 18 states
//!
//! In the library's own order, which is also the bit order of the table:
//!
//! ```text
//!  0 isInsert           9 wasLast
//!  1 isUpdate          10 sortParamsChanged
//!  2 isDelete          11 wasInResult
//!  3 hasLimit          12 wasSortedBeforeFirst
//!  4 isFindOne         13 wasSortedAfterLast
//!  5 hasSkip           14 isSortedBeforeFirst
//!  6 wasResultsEmpty   15 isSortedAfterLast
//!  7 wasLimitReached   16 wasMatching
//!  8 wasFirst          17 doesMatchNow
//! ```
//!
//! # The actions
//!
//! Only two are decided *behaviourally* here rather than by table lookup:
//! `doNothing` and `runFullQueryAgain`. The other fourteen are transformations on the
//! result array, implemented in [`crate::apply`]. Their names are kept identical to
//! upstream's so a reader can diff this file against `src/actions/index.ts`.

use std::cmp::Ordering;

use dxdb_storage::{ChangeEvent, DocumentData, Operation};

/// What to do with a change event.
///
/// The names are `event-reduce-js`'s `ActionName`s, in its order, so the recorded table
/// indexes directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// The event cannot affect this query's result.
    DoNothing,
    /// Insert the document at the front of the result.
    InsertFirst,
    /// Insert the document at the end of the result.
    InsertLast,
    /// Remove the first result.
    RemoveFirstItem,
    /// Remove the last result.
    RemoveLastItem,
    /// Remove the first result and append the changed document.
    RemoveFirstInsertLast,
    /// Remove the last result and prepend the changed document.
    RemoveLastInsertFirst,
    /// Remove the first result and prepend the changed document.
    RemoveFirstInsertFirst,
    /// Remove the last result and append the changed document.
    RemoveLastInsertLast,
    /// Remove an existing result at its sort position.
    RemoveExisting,
    /// Replace an existing result in place.
    ReplaceExisting,
    /// Only the truth-table generator produces this; never returned by the BDD.
    AlwaysWrong,
    /// Insert the document at its sorted position.
    InsertAtSortPosition,
    /// Remove the existing result and insert the document at its sorted position.
    RemoveExistingAndInsertAtSortPosition,
    /// Insert at the sort position and drop the last result.
    InsertAtSortPositionRemoveLast,
    /// Insert at the sort position and drop the first result.
    InsertAtSortPositionRemoveFirst,
    /// The event is not sufficient; run the whole query again.
    RunFullQueryAgain,
    /// The BDD produced an index the library does not know.
    UnknownAction,
}

impl Action {
    /// All actions in the library's order, which is the table's alphabet.
    pub const ALL: [Action; 18] = [
        Action::DoNothing,
        Action::InsertFirst,
        Action::InsertLast,
        Action::RemoveFirstItem,
        Action::RemoveLastItem,
        Action::RemoveFirstInsertLast,
        Action::RemoveLastInsertFirst,
        Action::RemoveFirstInsertFirst,
        Action::RemoveLastInsertLast,
        Action::RemoveExisting,
        Action::ReplaceExisting,
        Action::AlwaysWrong,
        Action::InsertAtSortPosition,
        Action::RemoveExistingAndInsertAtSortPosition,
        Action::InsertAtSortPositionRemoveLast,
        Action::InsertAtSortPositionRemoveFirst,
        Action::RunFullQueryAgain,
        Action::UnknownAction,
    ];

    /// The library's name for this action.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::DoNothing => "doNothing",
            Action::InsertFirst => "insertFirst",
            Action::InsertLast => "insertLast",
            Action::RemoveFirstItem => "removeFirstItem",
            Action::RemoveLastItem => "removeLastItem",
            Action::RemoveFirstInsertLast => "removeFirstInsertLast",
            Action::RemoveLastInsertFirst => "removeLastInsertFirst",
            Action::RemoveFirstInsertFirst => "removeFirstInsertFirst",
            Action::RemoveLastInsertLast => "removeLastInsertLast",
            Action::RemoveExisting => "removeExisting",
            Action::ReplaceExisting => "replaceExisting",
            Action::AlwaysWrong => "alwaysWrong",
            Action::InsertAtSortPosition => "insertAtSortPosition",
            Action::RemoveExistingAndInsertAtSortPosition => {
                "removeExistingAndInsertAtSortPosition"
            }
            Action::InsertAtSortPositionRemoveLast => "insertAtSortPositionRemoveLast",
            Action::InsertAtSortPositionRemoveFirst => "insertAtSortPositionRemoveFirst",
            Action::RunFullQueryAgain => "runFullQueryAgain",
            Action::UnknownAction => "unknownAction",
        }
    }

    /// The index in [`Action::ALL`].
    pub fn index(self) -> usize {
        // `ALL` is in the same order as the enum's discriminants.
        self as usize
    }

    /// The action for an index, or `UnknownAction` outside the range.
    pub fn from_index(index: usize) -> Action {
        Action::ALL
            .get(index)
            .copied()
            .unwrap_or(Action::UnknownAction)
    }

    /// True for the two verdicts that do not transform the result array.
    pub fn is_verdict(self) -> bool {
        matches!(
            self,
            Action::DoNothing | Action::RunFullQueryAgain | Action::UnknownAction
        )
    }
}

/// The 18 booleans the decision is a function of.
///
/// A struct rather than a bitmask so a failing case names the state, and so the table's bit
/// order is written down once, in [`States::bits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct States {
    /// The event is an insert.
    pub is_insert: bool,
    /// The event is an update.
    pub is_update: bool,
    /// The event is a delete.
    pub is_delete: bool,
    /// The query has a limit.
    pub has_limit: bool,
    /// The query's limit is exactly 1 (`findOne`).
    pub is_find_one: bool,
    /// The query has a skip greater than zero.
    pub has_skip: bool,
    /// The previous result was empty.
    pub was_results_empty: bool,
    /// The previous result had reached the limit.
    pub was_limit_reached: bool,
    /// The changed document was the first result.
    pub was_first: bool,
    /// The changed document was the last result.
    pub was_last: bool,
    /// A sort field changed.
    pub sort_params_changed: bool,
    /// The changed document was in the result set.
    pub was_in_result: bool,
    /// The previous state sorted before the first result.
    pub was_sorted_before_first: bool,
    /// The previous state sorted after the last result.
    pub was_sorted_after_last: bool,
    /// The new state sorts before the first result.
    pub is_sorted_before_first: bool,
    /// The new state sorts after the last result.
    pub is_sorted_after_last: bool,
    /// The previous state matched the selector.
    pub was_matching: bool,
    /// The new state matches the selector.
    pub does_match_now: bool,
}

impl States {
    /// The state names, in the library's order (the table's bit order).
    pub const NAMES: [&'static str; 18] = [
        "isInsert",
        "isUpdate",
        "isDelete",
        "hasLimit",
        "isFindOne",
        "hasSkip",
        "wasResultsEmpty",
        "wasLimitReached",
        "wasFirst",
        "wasLast",
        "sortParamsChanged",
        "wasInResult",
        "wasSortedBeforeFirst",
        "wasSortedAfterLast",
        "isSortedBeforeFirst",
        "isSortedAfterLast",
        "wasMatching",
        "doesMatchNow",
    ];

    /// The states as bits, least-significant first in [`States::NAMES`] order.
    pub fn bits(self) -> [bool; 18] {
        [
            self.is_insert,
            self.is_update,
            self.is_delete,
            self.has_limit,
            self.is_find_one,
            self.has_skip,
            self.was_results_empty,
            self.was_limit_reached,
            self.was_first,
            self.was_last,
            self.sort_params_changed,
            self.was_in_result,
            self.was_sorted_before_first,
            self.was_sorted_after_last,
            self.is_sorted_before_first,
            self.is_sorted_after_last,
            self.was_matching,
            self.does_match_now,
        ]
    }

    /// The row index in the recorded table.
    pub fn table_index(self) -> usize {
        let mut index = 0usize;
        for (position, bit) in self.bits().into_iter().enumerate() {
            if bit {
                index |= 1 << position;
            }
        }
        index
    }

    /// Builds states from a row index, for table-driven tests.
    pub fn from_table_index(index: usize) -> States {
        let bit = |position: usize| (index >> position) & 1 == 1;
        States {
            is_insert: bit(0),
            is_update: bit(1),
            is_delete: bit(2),
            has_limit: bit(3),
            is_find_one: bit(4),
            has_skip: bit(5),
            was_results_empty: bit(6),
            was_limit_reached: bit(7),
            was_first: bit(8),
            was_last: bit(9),
            sort_params_changed: bit(10),
            was_in_result: bit(11),
            was_sorted_before_first: bit(12),
            was_sorted_after_last: bit(13),
            is_sorted_before_first: bit(14),
            is_sorted_after_last: bit(15),
            was_matching: bit(16),
            does_match_now: bit(17),
        }
    }
}

/// The recorded decision table, loaded from the committed oracle.
///
/// `None` when the table has not been installed, in which case every decision is
/// `run_full_query_again` — the conservative direction.
struct ActionTable {
    table: &'static str,
}

/// The committed table, embedded so a decision never depends on the filesystem at runtime.
///
/// It is generated by `/tmp/er/table.cjs` (recorded here, not committed, because
/// `xtask/vectors` is lane C's) from `event-reduce-js` 7.4.0 and copied into
/// `testdata/event-reduce-action-table.json`. The `table` field of that JSON is inlined
/// below by `build.rs`-free means: the file is included and parsed once.
static TABLE_JSON: &str = include_str!("../testdata/event-reduce-action-table.json");

fn table() -> &'static ActionTable {
    use std::sync::OnceLock;
    static TABLE: OnceLock<ActionTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        // The table is 262144 ASCII characters; extract it once. A malformed file makes the
        // table empty, and every decision then falls back to `run_full_query_again`.
        let table = extract_table(TABLE_JSON);
        ActionTable { table }
    })
}

/// Pulls the `"table":"..."` value out of the JSON without a parser dependency.
///
/// The table is pure ASCII with no escapes, which is why a scan is safe; a `serde_json`
/// dependency for one string would be a poor trade in a crate that otherwise needs none.
fn extract_table(json: &'static str) -> &'static str {
    const KEY: &str = "\"table\":\"";
    let Some(start) = json.find(KEY) else {
        return "";
    };
    let rest = &json[start + KEY.len()..];
    let Some(end) = rest.find('"') else {
        return "";
    };
    &rest[..end]
}

/// The decision for a recorded state combination.
///
/// Returns `RunFullQueryAgain` for an out-of-range row, which cannot happen for a valid
/// [`States`] but is the safe default.
pub fn action_for(states: States) -> Action {
    let table = table();
    let index = states.table_index();
    match table.table.as_bytes().get(index) {
        Some(byte) if *byte >= b'A' && *byte <= b'Z' => Action::from_index((byte - b'A') as usize),
        // Unknown row: re-run rather than guess.
        _ => Action::RunFullQueryAgain,
    }
}

/// Everything needed to decide, and to act, on one change event.
///
/// Mirrors the library's `QueryParams` plus `StateResolveFunctionInput`. The comparator and
/// matcher are function pointers rather than trait objects so a caller can hand in the
/// query crate's implementations directly.
pub struct ReduceParams<'a> {
    /// The primary key path.
    pub primary_key: &'a str,
    /// The query's skip.
    pub skip: usize,
    /// The query's limit.
    pub limit: Option<usize>,
    /// The fields the sort reads, in sort order. Never empty: the primary key is appended by
    /// normalisation, so a query with no explicit sort sorts by primary key.
    pub sort_fields: &'a [String],
    /// Compares two documents by the query's sort order.
    pub sort_comparator: &'a dyn Fn(&DocumentData, &DocumentData) -> Ordering,
    /// True when a document matches the query's selector.
    pub query_matcher: &'a dyn Fn(&DocumentData) -> bool,
}

impl ReduceParams<'_> {
    /// `hasLimit`: `!!queryParams.limit`.
    ///
    /// Note a `limit: 0` counts as **no** limit upstream (`!!0` is false), which looks like
    /// a bug and is upstream's behaviour; matching it keeps the decision table valid.
    fn has_limit(&self) -> bool {
        self.limit.is_some_and(|limit| limit != 0)
    }

    /// `isFindOne`: `queryParams.limit === 1`.
    fn is_find_one(&self) -> bool {
        self.limit == Some(1)
    }

    /// `hasSkip`: `!!skip && skip > 0`.
    fn has_skip(&self) -> bool {
        self.skip > 0
    }

    /// The primary value of a document.
    fn document_id<'a>(&self, document: &'a DocumentData) -> Option<&'a str> {
        document.id(self.primary_key).ok()
    }

    /// True when `document`'s primary value equals `id`.
    pub fn document_id_matches(&self, document: &DocumentData, id: &str) -> bool {
        self.document_id(document)
            .is_some_and(|candidate| candidate == id)
    }
}

/// Resolves the 18 states for one change event.
///
/// Ported one-for-one from `states/state-resolver.ts`. Each arm cites the upstream function
/// name so a reviewer can diff it.
pub fn resolve_states(
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    previous_results: &[DocumentData],
) -> States {
    let id = event.document_id.as_str();
    let previous = event.previous_document_data.as_ref();
    let document = Some(&event.document_data);

    // `wasFirst` / `wasLast`.
    let was_first = previous_results
        .first()
        .and_then(|first| params.document_id(first))
        .is_some_and(|first_id| first_id == id);
    let was_last = previous_results
        .last()
        .and_then(|last| params.document_id(last))
        .is_some_and(|last_id| last_id == id);

    // `wasInResult`.
    let was_in_result = previous_results.iter().any(|candidate| {
        params
            .document_id(candidate)
            .is_some_and(|candidate_id| candidate_id == id)
    });

    // `sortParamsChanged`: nothing to compare for a delete (`doc` absent -> false); an
    // insert (`prev` absent) counts as changed.
    let sort_params_changed = match (document, previous) {
        (Some(document), Some(previous)) => params.sort_fields.iter().any(|field| {
            let before = previous.get_object_path(field);
            let after = document.get_object_path(field);
            before != after
        }),
        (Some(_), None) => true,
        (None, _) => false,
    };

    // `wasSortedBeforeFirst` / `wasSortedAfterLast`.
    //
    // Both hold `true` when the changed document *is* the first/last result, because
    // comparing a document against itself can yield a non-deterministic order.
    let was_sorted_before_first = match (previous, previous_results.first()) {
        (Some(previous), Some(first)) => {
            if params
                .document_id(first)
                .is_some_and(|first_id| first_id == id)
            {
                true
            } else {
                (params.sort_comparator)(previous, first) == Ordering::Less
            }
        }
        _ => false,
    };
    let was_sorted_after_last = match (previous, previous_results.last()) {
        (Some(previous), Some(last)) => {
            if params
                .document_id(last)
                .is_some_and(|last_id| last_id == id)
            {
                true
            } else {
                (params.sort_comparator)(previous, last) == Ordering::Greater
            }
        }
        _ => false,
    };

    // `isSortedBeforeFirst` / `isSortedAfterLast`: when the changed document is itself the
    // first/last, compare against the neighbour instead; with no neighbour it is still
    // first/last.
    let is_sorted_before_first = match (document, previous_results.first()) {
        (Some(document), Some(first)) => {
            let against = if params.document_id_matches(first, id) {
                // "If there is no other document, it is still the first."
                match previous_results.get(1) {
                    Some(second) => Some(second),
                    None => {
                        return sorted_extreme_states(params, event, previous_results, true, false)
                    }
                }
            } else {
                Some(first)
            };
            against.is_some_and(|other| (params.sort_comparator)(document, other) == Ordering::Less)
        }
        _ => false,
    };
    let is_sorted_after_last = match (document, previous_results.last()) {
        (Some(document), Some(last)) => {
            let against = if params.document_id_matches(last, id) {
                match previous_results
                    .len()
                    .checked_sub(2)
                    .and_then(|position| previous_results.get(position))
                {
                    Some(penultimate) => Some(penultimate),
                    None => {
                        return sorted_extreme_states(params, event, previous_results, false, true)
                    }
                }
            } else {
                Some(last)
            };
            against
                .is_some_and(|other| (params.sort_comparator)(document, other) == Ordering::Greater)
        }
        _ => false,
    };

    States {
        is_insert: event.operation == Operation::Insert,
        is_update: event.operation == Operation::Update,
        is_delete: event.operation == Operation::Delete,
        has_limit: params.has_limit(),
        is_find_one: params.is_find_one(),
        has_skip: params.has_skip(),
        was_results_empty: previous_results.is_empty(),
        was_limit_reached: params
            .limit
            .is_some_and(|limit| limit != 0 && previous_results.len() >= limit),
        was_first,
        was_last,
        sort_params_changed,
        was_in_result,
        was_sorted_before_first,
        was_sorted_after_last,
        is_sorted_before_first,
        is_sorted_after_last,
        was_matching: previous.is_some_and(|previous| (params.query_matcher)(previous)),
        does_match_now: document.is_some_and(|document| (params.query_matcher)(document)),
    }
}

/// The states for the "the changed document is the only result" case.
///
/// When the changed document is both first and last, there is no neighbour to compare
/// against, and upstream returns `true` for whichever extreme is being asked about. This
/// computes the remaining states and pins the two sort-position states so the caller does not
/// have to thread flags through the main resolver.
fn sorted_extreme_states(
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    previous_results: &[DocumentData],
    is_first: bool,
    is_last: bool,
) -> States {
    let states = resolve_without_sort_position(params, event, previous_results);
    States {
        is_sorted_before_first: is_first || states.is_sorted_before_first,
        is_sorted_after_last: is_last || states.is_sorted_after_last,
        ..states
    }
}

/// Every state except the two sort-position ones.
fn resolve_without_sort_position(
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    previous_results: &[DocumentData],
) -> States {
    let id = event.document_id.as_str();
    let previous = event.previous_document_data.as_ref();
    let document = Some(&event.document_data);
    let sort_params_changed = match (document, previous) {
        (Some(document), Some(previous)) => params
            .sort_fields
            .iter()
            .any(|field| previous.get_object_path(field) != document.get_object_path(field)),
        (Some(_), None) => true,
        (None, _) => false,
    };
    States {
        is_insert: event.operation == Operation::Insert,
        is_update: event.operation == Operation::Update,
        is_delete: event.operation == Operation::Delete,
        has_limit: params.has_limit(),
        is_find_one: params.is_find_one(),
        has_skip: params.has_skip(),
        was_results_empty: previous_results.is_empty(),
        was_limit_reached: params
            .limit
            .is_some_and(|limit| limit != 0 && previous_results.len() >= limit),
        was_first: previous_results
            .first()
            .is_some_and(|first| params.document_id_matches(first, id)),
        was_last: previous_results
            .last()
            .is_some_and(|last| params.document_id_matches(last, id)),
        sort_params_changed,
        was_in_result: previous_results
            .iter()
            .any(|candidate| params.document_id_matches(candidate, id)),
        was_sorted_before_first: false,
        was_sorted_after_last: false,
        is_sorted_before_first: false,
        is_sorted_after_last: false,
        was_matching: previous.is_some_and(|previous| (params.query_matcher)(previous)),
        does_match_now: document.is_some_and(|document| (params.query_matcher)(document)),
    }
}

/// The verdict for one change event against a previous result set.
#[derive(Clone, Debug, PartialEq)]
pub enum EventReduceResult {
    /// The event cannot be applied incrementally; run the query again.
    RunFullQueryAgain,
    /// The event was applied (or provably irrelevant). `changed` is false when the result set
    /// is unchanged, so a subscriber need not emit.
    Updated {
        /// Whether the result set actually changed.
        changed: bool,
        /// The new result set, in order.
        results: Vec<DocumentData>,
    },
}

/// Decides and applies one event.
///
/// This is `calculateNewResults` (`src/event-reduce.ts:135`) without the RxQuery object: it
/// resolves the states, looks up the action, and applies it. An action that is not one of the
/// two verdicts is applied by [`apply_action`]; an unrecognised action re-runs instead, which
/// is the conservative direction.
pub fn reduce(
    params: &ReduceParams<'_>,
    event: &ChangeEvent,
    previous_results: &[DocumentData],
) -> EventReduceResult {
    let states = resolve_states(params, event, previous_results);
    let action = action_for(states);
    match action {
        Action::DoNothing => EventReduceResult::Updated {
            changed: false,
            results: previous_results.to_vec(),
        },
        Action::RunFullQueryAgain | Action::UnknownAction => EventReduceResult::RunFullQueryAgain,
        action => {
            let results = crate::apply::apply_action(action, params, event, previous_results);
            EventReduceResult::Updated {
                changed: true,
                results,
            }
        }
    }
}
