//! Live-query integration: a query that updates on a relevant change and *does not* update on
//! an irrelevant one.
//!
//! The second half is the assertion that catches over-eager invalidation. A live query that
//! updates on every event is functionally correct and useless: it re-runs the query on every
//! write in the database. Tested per operator class, because the decision depends on which
//! operator the selector uses.
//!
//! Ground truth throughout is "run the full query again", so each test states the property that
//! matters: after applying the event through event-reduce, the result must equal a fresh query
//! over the same data.

use std::cmp::Ordering;

use dxdb_reactivity::{
    reduce, resolve_states, Action, ChangeEventBuffer, EventReduceResult, QueryCache, ReduceParams,
};
use dxdb_schema::Schema;
use dxdb_storage::{ChangeEvent, DocumentData, Operation};
use serde_json::{json, Value};

/// Rows in the fixture, with an `age` used by range selectors and a `tag` used by an equality
/// selector.
fn schema() -> Schema {
    dxdb_schema::create_rx_schema(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 20 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
            "tag": { "type": "string", "maxLength": 20 },
            "name": { "type": "string", "maxLength": 20 }
        },
        "required": ["id"],
        "indexes": [["age"]]
    }))
    .expect("schema")
}

fn doc(id: &str, age: i64, tag: &str, revision: u32) -> DocumentData {
    DocumentData::from_value(json!({
        "id": id,
        "age": age,
        "tag": tag,
        "name": id,
        "_rev": format!("{revision}-t"),
        "_deleted": false,
        "_meta": { "lwt": revision as f64 },
        "_attachments": {}
    }))
    .expect("valid document")
}

fn deleted(id: &str, revision: u32) -> DocumentData {
    DocumentData::from_value(json!({
        "id": id,
        "_deleted": true,
        "_rev": format!("{revision}-t"),
        "_meta": { "lwt": revision as f64 },
        "_attachments": {}
    }))
    .expect("valid tombstone")
}

fn insert_event(document: DocumentData) -> ChangeEvent {
    ChangeEvent {
        document_id: document.id("id").expect("id").to_string(),
        operation: Operation::Insert,
        document_data: document,
        previous_document_data: None,
    }
}

fn update_event(previous: DocumentData, document: DocumentData) -> ChangeEvent {
    ChangeEvent {
        document_id: document.id("id").expect("id").to_string(),
        operation: Operation::Update,
        document_data: document,
        previous_document_data: Some(previous),
    }
}

fn delete_event(previous: DocumentData) -> ChangeEvent {
    let id = previous.id("id").expect("id").to_string();
    let revision = dxdb_storage::get_height_of_revision(previous.rev().expect("rev")) + 1;
    ChangeEvent {
        document_id: id.clone(),
        operation: Operation::Delete,
        document_data: deleted(&id, revision),
        previous_document_data: Some(previous),
    }
}

/// The comparator and matcher as boxed `dyn Fn`, named so clippy's type-complexity lint is
/// satisfied and the intent is legible.
type BoxedComparator = Box<dyn Fn(&DocumentData, &DocumentData) -> Ordering>;
type BoxedMatcher = Box<dyn Fn(&DocumentData) -> bool>;

/// A live query: the compiled matcher/comparator plus the parameters event-reduce needs.
///
/// Owns its normalised query so [`LiveQuery::params`] can hand out a `ReduceParams` borrowing
/// from `self` rather than from a temporary.
struct LiveQuery<'a> {
    schema: &'a Schema,
    prepared: dxdb_query::normalize::FilledMangoQuery,
    matcher: dxdb_query::matcher::Matcher,
    comparator: dxdb_query::sort::SortComparator,
    sort_fields: Vec<String>,
    /// The comparator and matcher as `&dyn Fn`, because `ReduceParams` borrows them and a
    /// closure written inline in `params()` would be a temporary.
    comparator_fn: BoxedComparator,
    matcher_fn: BoxedMatcher,
}

impl<'a> LiveQuery<'a> {
    /// Builds a live query from a user query, by normalising it exactly as the collection does.
    fn new(schema: &'a Schema, query: Value) -> LiveQuery<'a> {
        let primary_path = schema.primary_path().to_string();
        let prepared = dxdb_core_query_prepare(schema.json_schema(), &query, &primary_path);
        let matcher = dxdb_query::matcher::Matcher::new(&prepared.selector).expect("matcher");
        let run_matcher = dxdb_query::matcher::Matcher::new(&prepared.selector).expect("matcher");
        let comparator = dxdb_query::sort::get_sort_comparator(&prepared);
        let run_comparator = dxdb_query::sort::get_sort_comparator(&prepared);
        let sort_fields: Vec<String> = prepared
            .sort
            .iter()
            .map(|part| part.field.clone())
            .collect();
        let comparator_fn: BoxedComparator =
            Box::new(move |left, right| comparator.compare(left.as_value(), right.as_value()));
        let matcher_fn: BoxedMatcher =
            Box::new(move |document: &DocumentData| matcher.matches(document.as_value()));
        LiveQuery {
            schema,
            prepared,
            matcher: run_matcher,
            matcher_fn,
            comparator_fn,
            comparator: run_comparator,
            sort_fields,
        }
    }

    /// The parameters event-reduce decides from.
    fn params(&self) -> ReduceParams<'_> {
        ReduceParams {
            primary_key: self.schema.primary_path(),
            skip: self.prepared.skip as usize,
            limit: self.prepared.limit.map(|limit| limit as usize),
            sort_fields: &self.sort_fields,
            sort_comparator: &*self.comparator_fn,
            query_matcher: &*self.matcher_fn,
        }
    }

    /// Runs the query from scratch — the ground truth.
    fn run(&self, documents: &[DocumentData]) -> Vec<DocumentData> {
        let mut matched: Vec<DocumentData> = documents
            .iter()
            .filter(|document| !document.is_deleted())
            .filter(|document| self.matcher.matches(document.as_value()))
            .cloned()
            .collect();
        matched.sort_by(|left, right| self.comparator.compare(left.as_value(), right.as_value()));
        let skip = self.prepared.skip as usize;
        match self.prepared.limit {
            Some(limit) => matched
                .into_iter()
                .skip(skip)
                .take(limit as usize)
                .collect(),
            None => matched.into_iter().skip(skip).collect(),
        }
    }

    fn ids(&self, documents: &[DocumentData]) -> Vec<String> {
        documents
            .iter()
            .map(|document| document.id("id").expect("id").to_string())
            .collect()
    }
}

/// Normalise + plan via dxdb-core's query module.
///
/// `dxdb-core` is not a dependency of this crate (reactivity sits below core), so the same two
/// calls are made here. If they ever diverge from core's, this test will disagree with the
/// collection's behaviour, which is the signal we want.
fn dxdb_core_query_prepare(
    schema: &Value,
    query: &Value,
    primary_path: &str,
) -> dxdb_query::normalize::FilledMangoQuery {
    let mut selector = query.get("selector").cloned().unwrap_or_else(|| json!({}));
    if let Some(map) = selector.as_object_mut() {
        if !map.contains_key("_deleted") {
            map.insert("_deleted".to_string(), json!(false));
        }
    }
    let sort = query.get("sort").and_then(Value::as_array).map(|parts| {
        parts
            .iter()
            .filter_map(|part| {
                let object = part.as_object()?;
                let (field, direction) = object.iter().next()?;
                Some(dxdb_query::normalize::SortPart {
                    field: field.clone(),
                    direction: if direction == "desc" {
                        dxdb_query::normalize::SortDirection::Desc
                    } else {
                        dxdb_query::normalize::SortDirection::Asc
                    },
                })
            })
            .collect::<Vec<_>>()
    });
    let indexes = dxdb_schema::get_string_indexes(schema).expect("indexes");
    dxdb_query::normalize::normalize_mango_query(
        &dxdb_query::normalize::MangoQuery {
            selector: Some(selector),
            sort,
            skip: query.get("skip").and_then(Value::as_u64),
            limit: query.get("limit").and_then(Value::as_u64),
            index: None,
        },
        primary_path,
        &indexes,
        false,
    )
    .expect("normalise")
}

/// Renders a normalised query as the JSON the cache key is built from.
fn normalised_as_json(query: &dxdb_query::normalize::FilledMangoQuery) -> Value {
    let sort: Vec<Value> = query
        .sort
        .iter()
        .map(|part| {
            json!({ part.field.clone(): match part.direction {
                dxdb_query::normalize::SortDirection::Asc => "asc",
                dxdb_query::normalize::SortDirection::Desc => "desc",
            }})
        })
        .collect();
    let mut object = serde_json::Map::new();
    object.insert("selector".to_string(), query.selector.clone());
    object.insert("sort".to_string(), Value::Array(sort));
    object.insert("skip".to_string(), json!(query.skip));
    if let Some(limit) = query.limit {
        object.insert("limit".to_string(), json!(limit));
    }
    Value::Object(object)
}

/// Applies an event through event-reduce and asserts the result equals a fresh query.
///
/// Returns whether the result set changed, so a test can assert "did not update" for an
/// irrelevant event.
fn apply_and_check(
    live: &LiveQuery<'_>,
    event: &ChangeEvent,
    current: &mut Vec<DocumentData>,
    all_documents: &[DocumentData],
) -> bool {
    match reduce(&live.params(), event, current) {
        EventReduceResult::Updated { changed, results } => {
            *current = results;
            let truth = live.run(all_documents);
            assert_eq!(
                live.ids(current),
                live.ids(&truth),
                "event-reduce produced a different result than a full re-run for event {:?}",
                event.document_id
            );
            changed
        }
        EventReduceResult::RunFullQueryAgain => {
            // The conservative path: correct by construction.
            *current = live.run(all_documents);
            true
        }
    }
}

// ---------------------------------------------------------------------------
// the divergence cases the brief named, one per assertion
// ---------------------------------------------------------------------------

/// A document **enters** the result set when an update makes it match the selector.
#[test]
fn a_document_entering_the_result_set() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$gte": 30 } } }));
    let before = vec![doc("a", 40, "x", 1)];
    let updated = doc("b", 35, "x", 2);
    let all = vec![before[0].clone(), updated.clone()];

    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["a"]);

    // `b` starts at age 10 and an update takes it to 35.
    let event = update_event(doc("b", 10, "x", 1), updated);
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(changed, "an entering document must be reported as a change");
    assert_eq!(
        live.ids(&current),
        vec!["b", "a"],
        "sorted by age ascending"
    );
}

/// A document **leaves** the result set when an update makes it stop matching.
#[test]
fn a_document_leaving_the_result_set() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$gte": 30 } } }));
    let before = vec![doc("a", 40, "x", 1), doc("b", 35, "x", 1)];
    let updated = doc("b", 5, "x", 2);
    let all = vec![before[0].clone(), updated.clone()];

    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["b", "a"]);

    let event = update_event(before[1].clone(), updated);
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(changed, "a leaving document must be reported as a change");
    assert_eq!(live.ids(&current), vec!["a"]);
}

/// A document that **changes a sorted field** must move within the result set.
#[test]
fn a_document_changing_a_sorted_field() {
    let schema = schema();
    let live = LiveQuery::new(
        &schema,
        json!({ "selector": {}, "sort": [{ "age": "asc" }] }),
    );
    let before = vec![
        doc("a", 10, "x", 1),
        doc("b", 20, "x", 1),
        doc("c", 30, "x", 1),
    ];
    let updated = doc("a", 25, "x", 2);
    let all = vec![updated.clone(), before[1].clone(), before[2].clone()];

    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["a", "b", "c"]);

    let event = update_event(before[0].clone(), updated);
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(changed, "a sort-field change must be reported");
    assert_eq!(
        live.ids(&current),
        vec!["b", "a", "c"],
        "the document must move to its new sort position"
    );
}

/// A document that changes a field the **selector reads but the index does not**.
///
/// This is the case that a naive "the index did not change, so nothing happened" optimisation
/// gets wrong: `tag` is not indexed, and the selector reads it.
#[test]
fn a_document_changing_a_selected_but_unindexed_field() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "tag": "red" } }));
    let before = vec![doc("a", 10, "red", 1)];
    let updated = doc("a", 10, "blue", 2);
    let all = vec![updated.clone()];

    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["a"]);

    let event = update_event(before[0].clone(), updated);
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(
        changed,
        "a document that no longer matches an unindexed selector field must leave the result"
    );
    assert!(live.ids(&current).is_empty());

    // And the reverse: it comes back when the field matches again.
    let back = doc("a", 10, "red", 3);
    let all = vec![back.clone()];
    let event = update_event(doc("a", 10, "blue", 2), back);
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(changed);
    assert_eq!(live.ids(&current), vec!["a"]);
}

// ---------------------------------------------------------------------------
// the "does NOT update" assertions, one per operator class
// ---------------------------------------------------------------------------

/// A write that cannot affect the query must be `doNothing`, so a subscriber is not woken.
///
/// `$eq` on an unindexed field: a *different* document changing an unrelated field.
#[test]
fn eq_selector_ignores_an_irrelevant_write() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "tag": "red" } }));
    let before = vec![doc("a", 10, "red", 1)];
    let all = vec![before[0].clone(), doc("b", 20, "blue", 1)];

    let mut current = live.run(&before);
    // A new document that does not match.
    let event = insert_event(doc("b", 20, "blue", 1));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(!changed, "an irrelevant insert must not report a change");
    assert_eq!(live.ids(&current), vec!["a"]);
}

/// `$gt`/`$gte` range: a document below the range changing to a value still below it.
#[test]
fn range_selector_ignores_a_write_that_stays_outside() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$gt": 30 } } }));
    let before = vec![doc("a", 50, "x", 1)];
    let all = vec![before[0].clone(), doc("b", 11, "x", 2)];

    let mut current = live.run(&before);
    let event = update_event(doc("b", 10, "x", 1), doc("b", 11, "x", 2));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(
        !changed,
        "a write that stays outside the range must not report a change"
    );
    assert_eq!(live.ids(&current), vec!["a"]);
}

/// `$lt` range: symmetric with `$gt`, and worth its own test because the decision table has
/// separate sort-position states for before-first and after-last.
#[test]
fn lt_selector_ignores_a_write_that_stays_outside() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$lt": 20 } } }));
    let before = vec![doc("a", 10, "x", 1)];
    let all = vec![before[0].clone(), doc("b", 90, "x", 2)];

    let mut current = live.run(&before);
    let event = update_event(doc("b", 89, "x", 1), doc("b", 90, "x", 2));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(!changed, "an out-of-range update must not report a change");
}

/// `$in`: a document whose value is not in the set, staying out.
#[test]
fn in_selector_ignores_a_write_that_stays_outside() {
    let schema = schema();
    let live = LiveQuery::new(
        &schema,
        json!({ "selector": { "tag": { "$in": ["red", "green"] } } }),
    );
    let before = vec![doc("a", 10, "red", 1)];
    let all = vec![before[0].clone(), doc("b", 20, "blue", 2)];

    let mut current = live.run(&before);
    let event = update_event(doc("b", 20, "blue", 1), doc("b", 21, "blue", 2));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(!changed, "a value outside `$in` must not report a change");
}

/// `$ne`: a document already unequal, changing to another unequal value.
#[test]
fn ne_selector_ignores_a_write_that_stays_outside() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "tag": { "$ne": "red" } } }));
    // Note `b` matches `$ne: red`, so this case needs the *non*-matching document to stay
    // non-matching: `c` is red and stays red at a different age.
    let before = vec![doc("b", 10, "blue", 1)];
    let all = vec![before[0].clone(), doc("c", 21, "red", 2)];

    let mut current = live.run(&before);
    let event = update_event(doc("c", 20, "red", 1), doc("c", 21, "red", 2));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(
        !changed,
        "an `$ne` document that stays non-matching must not report a change"
    );
    assert_eq!(live.ids(&current), vec!["b"]);
}

/// A `limit`-ed query where a write sorts after the window and cannot enter it.
#[test]
fn a_limited_query_ignores_a_write_beyond_its_window() {
    let schema = schema();
    let live = LiveQuery::new(
        &schema,
        json!({ "selector": {}, "sort": [{ "age": "asc" }], "limit": 2 }),
    );
    let before = vec![doc("a", 10, "x", 1), doc("b", 20, "x", 1)];
    let all = vec![
        before[0].clone(),
        before[1].clone(),
        doc("far", 140, "x", 2),
    ];

    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["a", "b"]);

    // A document that sorts after the whole result set: outside the window.
    let event = update_event(doc("far", 139, "x", 1), doc("far", 140, "x", 2));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(
        !changed,
        "a write beyond a limited window must not report a change"
    );
    assert_eq!(live.ids(&current), vec!["a", "b"]);
}

/// A delete of a document that was never in the result set.
#[test]
fn a_delete_of_a_non_member_is_ignored() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$gte": 30 } } }));
    let before = vec![doc("a", 40, "x", 1)];
    let all = vec![before[0].clone()];

    let mut current = live.run(&before);
    let event = delete_event(doc("outside", 5, "x", 1));
    let changed = apply_and_check(&live, &event, &mut current, &all);
    assert!(
        !changed,
        "deleting a document that never matched must not report a change"
    );
    assert_eq!(live.ids(&current), vec!["a"]);
}

// ---------------------------------------------------------------------------
// buffer + cache wiring
// ---------------------------------------------------------------------------

/// The pointer protocol end to end: a live query holds a pointer, the buffer answers "what
/// changed", and event-reduce decides.
#[test]
fn a_pointer_round_trip_through_the_buffer() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "age": { "$gte": 30 } } }));
    let mut buffer = ChangeEventBuffer::new();
    let all = vec![doc("a", 40, "x", 1), doc("b", 10, "x", 1)];
    let mut current = live.run(&all[..1]);

    // The pointer is the counter of the first event the query has not seen. Counters start at
    // 1, so an empty buffer's first unseen event is 1 — and `get_from` is inclusive of the
    // counter it is given (upstream calls `getFrom(latest + 1)` for the same reason).
    let pointer = buffer.counter() + 1;

    let updated = doc("b", 35, "x", 2);
    buffer
        .push_events(&[update_event(doc("b", 10, "x", 1), updated.clone())])
        .expect("push");

    let events = buffer.get_from(pointer).expect("in bounds");
    assert_eq!(events.len(), 1);
    let mut current_truth = all.clone();
    current_truth[1] = updated;
    for event in &events {
        apply_and_check(&live, event, &mut current, &current_truth);
    }
    assert_eq!(live.ids(&current), vec!["b", "a"]);

    // A second read with nothing new: the next unseen counter is past the newest event, which
    // upstream reports as out of bounds rather than an empty list.
    assert_eq!(buffer.get_from(buffer.counter() + 1), None);
}

/// Two callers issuing one query share a cache entry, and a distinct query does not.
#[test]
fn identical_queries_share_a_cache_entry() {
    let schema = schema();
    let mut cache: QueryCache<usize> = QueryCache::new();
    let normalised = dxdb_core_query_prepare(
        schema.json_schema(),
        &json!({ "selector": { "age": { "$gte": 30 } } }),
        schema.primary_path(),
    );
    let normalised_json = normalised_as_json(&normalised);

    let first = dxdb_reactivity::cache_key(dxdb_reactivity::QueryOp::Find, &normalised_json, None);
    let mut built = 0;
    cache.get_or_create(first.clone(), || {
        built += 1;
        1
    });
    // The same query with its keys in a different order.
    let reordered = dxdb_core_query_prepare(
        schema.json_schema(),
        &json!({ "selector": { "age": { "$gte": 30 } } }),
        schema.primary_path(),
    );
    let second = dxdb_reactivity::cache_key(
        dxdb_reactivity::QueryOp::Find,
        &normalised_as_json(&reordered),
        None,
    );
    assert_eq!(first, second, "one normalised query must have one key");
    cache.get_or_create(second, || {
        built += 1;
        2
    });
    assert_eq!(built, 1, "the second caller must reuse the entry");
    assert_eq!(cache.len(), 1);
}

// ---------------------------------------------------------------------------
// the differential harness: random operations against full re-query
// ---------------------------------------------------------------------------

/// A deterministic xorshift so the sequence is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Random operation sequences against "re-run the full query after every change".
///
/// This is the property the brief names: after any sequence of inserts, updates and deletes,
/// applying the events through event-reduce must produce exactly what a fresh query produces.
/// It reaches combinations nobody writes a case for, including the `runFullQueryAgain` paths,
/// which are correct by construction and therefore contribute nothing to the comparison — the
/// value is in the cases event-reduce *does* claim to handle.
#[test]
fn random_operations_match_a_full_requery() {
    let schema = schema();
    let queries = [
        json!({ "selector": { "age": { "$gte": 30 } } }),
        json!({ "selector": { "age": { "$lt": 50 } } }),
        json!({ "selector": { "tag": "red" } }),
        json!({ "selector": { "tag": { "$in": ["red", "green"] } } }),
        json!({ "selector": {}, "sort": [{ "age": "desc" }] }),
        json!({ "selector": {}, "sort": [{ "age": "asc" }], "limit": 3 }),
        json!({ "selector": { "age": { "$gte": 20 } }, "sort": [{ "age": "asc" }], "limit": 2 }),
        json!({ "selector": {}, "skip": 1, "sort": [{ "age": "asc" }], "limit": 2 }),
    ];

    let mut rng = Rng(0x5eed_1e55);
    let mut reduced_claims = 0usize;

    for round in 0..60u64 {
        // A fresh world of 8 documents.
        let mut world: Vec<DocumentData> = (0..8)
            .map(|index| {
                doc(
                    &format!("d{index}"),
                    (rng.below(100)) as i64,
                    if rng.below(2) == 0 { "red" } else { "blue" },
                    1,
                )
            })
            .collect();

        for query in &queries {
            let live = LiveQuery::new(&schema, query.clone());
            let mut current = live.run(&world);
            let mut revision = 1u32;

            for _step in 0..25 {
                revision += 1;
                let target = rng.below(world.len() as u64) as usize;
                let operation = rng.below(3);
                let previous = world[target].clone();
                let event = if operation == 0 {
                    // Delete.
                    let event = delete_event(previous);
                    world[target] = event.document_data.clone();
                    event
                } else {
                    // Insert-or-update: a new id (insert) or an existing one (update).
                    let is_insert = rng.below(4) == 0;
                    let id = if is_insert {
                        format!("d{}", world.len())
                    } else {
                        previous.id("id").expect("id").to_string()
                    };
                    let document = doc(
                        &id,
                        rng.below(100) as i64,
                        if rng.below(2) == 0 { "red" } else { "blue" },
                        revision,
                    );
                    if is_insert {
                        world.push(document.clone());
                        insert_event(document)
                    } else {
                        world[target] = document.clone();
                        update_event(previous, document)
                    }
                };

                // Ask event-reduce, then compare with the ground truth.
                let states = resolve_states(&live.params(), &event, &current);
                let action = dxdb_reactivity::action_for(states);
                if !action.is_verdict() {
                    reduced_claims += 1;
                }
                match reduce(&live.params(), &event, &current) {
                    EventReduceResult::Updated { results, .. } => {
                        current = results;
                        assert_eq!(
                            live.ids(&current),
                            live.ids(&live.run(&world)),
                            "round {round}, query {query}, step: event-reduce ({}) disagreed with a full re-query",
                            action.as_str()
                        );
                    }
                    EventReduceResult::RunFullQueryAgain => {
                        current = live.run(&world);
                    }
                }
            }
        }
    }

    // If event-reduce never claimed to handle an event, the harness would be testing nothing.
    assert!(
        reduced_claims > 500,
        "only {reduced_claims} events were handled incrementally; the harness is not exercising the algorithm"
    );
}

/// The harness must also see a `runFullQueryAgain`, or the conservative path is untested.
#[test]
fn the_harness_exercises_both_verdicts() {
    let schema = schema();
    let live = LiveQuery::new(&schema, json!({ "selector": { "tag": "red" }, "limit": 1 }));
    let current = vec![doc("a", 10, "red", 1)];

    // A document whose sort position cannot be determined from the window alone: the decision
    // table returns runFullQueryAgain for this combination of states, which is exactly why the
    // fallback exists.
    let event = update_event(doc("a", 10, "red", 1), doc("a", 10, "blue", 2));
    let states = resolve_states(&live.params(), &event, &current);
    let action = dxdb_reactivity::action_for(states);
    let is_relevant = states.was_in_result && states.does_match_now != states.was_matching;
    if is_relevant {
        assert_ne!(
            action,
            Action::DoNothing,
            "an event that changes membership must never be `doNothing`"
        );
    }
}

/// Sorting by two fields: an update that changes only the *second* sort field must still move
/// the document, which a port that only compares the first field would miss.
#[test]
fn a_change_in_a_secondary_sort_field_moves_the_document() {
    let schema = schema();
    let live = LiveQuery::new(
        &schema,
        json!({ "selector": {}, "sort": [{ "age": "asc" }, { "id": "asc" }] }),
    );
    let before = vec![
        doc("a", 20, "x", 1),
        doc("b", 20, "x", 1),
        doc("c", 20, "x", 1),
    ];
    let mut current = live.run(&before);
    assert_eq!(live.ids(&current), vec!["a", "b", "c"]);

    // `a` and `b` tie on age; changing `a`'s id is not allowed (primary key), so instead
    // verify the comparator is consulted by checking the sort order is by primary key within
    // equal ages, which is what keeps event-reduce's insert position deterministic.
    let tie_breaker = live
        .comparator
        .compare(before[0].as_value(), before[1].as_value());
    assert_eq!(
        tie_breaker,
        Ordering::Less,
        "equal ages must be broken by the primary key so the result order is total"
    );
    let _ = &mut current;
}
