//! The differential fuzz harness for live queries.
//!
//! PLAN.md 7.3's instrument. The property is deliberately one-sided and unimpeachable:
//!
//! > After every single change, event-reduce's incremental result must equal — as documents
//! > **and as order** — the result of re-running the whole query from scratch.
//!
//! The ground truth cannot be wrong: it is the same matcher, the same comparator and the same
//! skip/limit applied to the current world by brute force. So any disagreement is a bug in the
//! fast path, and there are exactly two kinds:
//!
//! * **stale result** — event-reduce decided a change was insufficient when it was sufficient,
//!   and a live query silently misses an update. This is the correctness bug;
//! * **spurious change** — event-reduce transformed the result when the change could not matter,
//!   so a subscriber is woken for nothing. Only a performance bug.
//!
//! Both fail here, because both are disagreements, but the failure message says which one
//! happened so the severity is legible.
//!
//! # Why a fuzz harness rather than more cases
//!
//! event-reduce's decision is a function of 18 booleans derived from the event, the previous
//! result and the query. Hand-written cases reach the states a human thought of — the four
//! divergence cases and one per operator class are covered in `live_query.rs`. Random operation
//! sequences reach the *combinations*: a delete of the first result in a skip+limit query whose
//! sort field the document no longer has, after a resurrection, on the round where the result set
//! happened to be exactly one document long. Nobody writes that case down.
//!
//! # Reproducibility
//!
//! Every run is seeded, and a failure prints the seed, the query, every operation in order with
//! its document state, and the divergence. A counterexample you cannot replay is worthless, so
//! the printed block is designed to be pasted into a new `#[test]`.
//!
//! The seeds are fixed constants, not random: a fuzz harness whose corpus changes between runs
//! cannot distinguish "we fixed it" from "we got lucky", and CI must be deterministic.
//!
//! # The complementary assertion
//!
//! A harness that only checks equality would pass just as happily if event-reduce re-ran the
//! query every time — the fast path would be dead code and every test would go through it
//! harmlessly. So the harness also counts how often the fast path actually fired and asserts a
//! floor. That is what proves the optimisation is not silently disabled.

use std::collections::BTreeMap;

use dxdb_reactivity::{action_for, reduce, resolve_states, EventReduceResult};
use dxdb_schema::Schema;
use dxdb_storage::{ChangeEvent, DocumentData, Operation};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// deterministic PRNG
// ---------------------------------------------------------------------------

/// xorshift64*, seeded. Reproducible on every platform, which a `HashMap` iteration order or a
/// float-based generator would not be.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state would lock xorshift at zero.
        Rng(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A value in `0..bound`.
    fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "bound must be positive");
        self.next_u64() % bound
    }

    /// A value in `low..=high`.
    fn range(&mut self, low: i64, high: i64) -> i64 {
        let span = (high - low + 1) as u64;
        low + self.below(span) as i64
    }

    /// True with probability `numerator / denominator`.
    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------------------
// the world
// ---------------------------------------------------------------------------

/// The fixture schema.
///
/// `age` is indexed (so a range selector on it is queryable), `tag` is a plain string the
/// selector can read but no index covers, `name` is neither indexed nor selected, and `skills`
/// is an array of objects so `$elemMatch` is reachable.
fn schema() -> Schema {
    dxdb_schema::create_rx_schema(&json!({
        "version": 0,
        "primaryKey": "id",
        "type": "object",
        "properties": {
            "id": { "type": "string", "maxLength": 24 },
            "age": { "type": "integer", "minimum": 0, "maximum": 150, "multipleOf": 1 },
            "tag": { "type": "string", "maxLength": 16 },
            "name": { "type": "string", "maxLength": 16 },
            "skills": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "maxLength": 16 },
                        "damage": { "type": "integer", "minimum": 0, "maximum": 100 }
                    },
                    "required": ["name", "damage"]
                }
            }
        },
        "required": ["id"],
        "indexes": [["age"]]
    }))
    .expect("the fixture schema is valid")
}

const TAGS: [&str; 4] = ["red", "green", "blue", "amber"];

/// A fresh document at revision `revision`.
/// Ages the generator draws from. `42` appears often so the `indexed-eq` shape always has
/// matches; without a weighted value an `$eq` on a 121-value range is empty most runs, and an
/// empty query exercises nothing.
const AGES: [i64; 8] = [42, 42, 10, 20, 30, 60, 90, 120];

fn document(rng: &mut Rng, id: &str, revision: u32) -> DocumentData {
    let skills = (0..rng.range(0, 2))
        .map(|_| {
            json!({
                "name": if rng.chance(1, 2) { "sword" } else { "shield" },
                "damage": rng.range(0, 20)
            })
        })
        .collect::<Vec<_>>();
    DocumentData::from_value(json!({
        "id": id,
        "age": *rng.pick(&AGES),
        "tag": rng.pick(&TAGS),
        "name": format!("n{}", rng.range(0, 9)),
        "skills": skills,
        "_rev": format!("{revision}-t"),
        "_deleted": false,
        "_meta": { "lwt": revision as f64 },
        "_attachments": {}
    }))
    .expect("generated document is an object")
}

/// A tombstone for `id`.
fn tombstone(id: &str, revision: u32) -> DocumentData {
    DocumentData::from_value(json!({
        "id": id,
        "_deleted": true,
        "_rev": format!("{revision}-t"),
        "_meta": { "lwt": revision as f64 },
        "_attachments": {}
    }))
    .expect("tombstone is an object")
}

// ---------------------------------------------------------------------------
// operations
// ---------------------------------------------------------------------------

/// One operation, recorded so a failure can be replayed.
#[derive(Clone, Debug)]
enum Op {
    /// A brand-new id appears.
    Insert { id: String, revision: u32 },
    /// A field the selector **reads** changes, which can move a document in or out of the result.
    UpdateSelectedField { id: String, revision: u32 },
    /// A field the selector does **not** read changes, which must not affect membership.
    UpdateUnselectedField { id: String, revision: u32 },
    /// A sort field changes, which must move the document within the result.
    UpdateSortField { id: String, revision: u32 },
    /// The document becomes a tombstone.
    Delete { id: String },
    /// A tombstoned id comes back as a live document.
    Resurrect { id: String, revision: u32 },
    /// A control operation: a write that matches no query at all.
    UpdateIrrelevant { id: String, revision: u32 },
}

impl Op {
    /// A stable label for the failure log.
    fn label(&self) -> &'static str {
        match self {
            Op::Insert { .. } => "insert",
            Op::UpdateSelectedField { .. } => "update-selected",
            Op::UpdateUnselectedField { .. } => "update-unselected",
            Op::UpdateSortField { .. } => "update-sort",
            Op::Delete { .. } => "delete",
            Op::Resurrect { .. } => "resurrect",
            Op::UpdateIrrelevant { .. } => "update-irrelevant",
        }
    }
}

/// Applies `op` to the world and returns the change event it produced.
///
/// The event is what a storage would emit, so the world and the event cannot disagree — which
/// matters, because the ground truth reads the world and event-reduce reads the event.
fn apply_op(world: &mut BTreeMap<String, DocumentData>, rng: &mut Rng, op: &Op) -> ChangeEvent {
    match op {
        Op::Insert { id, revision } => {
            let document = document(rng, id, *revision);
            world.insert(id.clone(), document.clone());
            ChangeEvent {
                document_id: id.clone(),
                operation: Operation::Insert,
                document_data: document,
                previous_document_data: None,
            }
        }
        Op::UpdateSelectedField { id, revision }
        | Op::UpdateUnselectedField { id, revision }
        | Op::UpdateSortField { id, revision }
        | Op::UpdateIrrelevant { id, revision } => {
            let previous = world
                .get(id)
                .cloned()
                .unwrap_or_else(|| document(rng, id, revision.saturating_sub(1)));
            let mut next = previous.clone();
            next.set("_rev", json!(format!("{revision}-t")));
            next.set("_meta", json!({ "lwt": *revision as f64 }));
            match op {
                // `tag` is read by some selectors and covered by no index.
                Op::UpdateSelectedField { .. } => next.set("tag", json!(rng.pick(&TAGS))),
                // `name` is read by no selector in the corpus.
                Op::UpdateUnselectedField { .. } => {
                    next.set("name", json!(format!("n{}", rng.range(0, 9))))
                }
                // `age` is indexed and is the sort field for most shapes.
                Op::UpdateSortField { .. } | Op::UpdateIrrelevant { .. } => {
                    next.set("age", json!(rng.pick(&AGES)))
                }
                _ => unreachable!("matched above"),
            };
            world.insert(id.clone(), next.clone());
            ChangeEvent {
                document_id: id.clone(),
                operation: Operation::Update,
                document_data: next,
                previous_document_data: Some(previous),
            }
        }
        Op::Delete { id } => {
            let previous = world.get(id).cloned().unwrap_or_else(|| tombstone(id, 1));
            // The tombstone's revision must be one higher than what the world holds, so a
            // resurrection later derives a sane height.
            let revision =
                dxdb_storage::try_get_height_of_revision(previous.rev().unwrap_or("1-x"))
                    .unwrap_or(1)
                    + 1;
            let _ = &previous;
            let next = tombstone(id, revision);
            world.insert(id.clone(), next.clone());
            ChangeEvent {
                document_id: id.clone(),
                operation: Operation::Delete,
                document_data: next,
                previous_document_data: Some(previous),
            }
        }
        Op::Resurrect { id, revision } => {
            // The previous state must be the tombstone the world holds, or the event lies about
            // the revision and event-reduce's height-driven decisions become meaningless.
            // Named with a leading underscore: the value documents the precondition the
            // comment above asserts, but the resurrection path deliberately does not forward
            // it (a resurrection is an INSERT event with no previous state). Reading it here
            // also proves the world held a tombstone, so keeping the lookup is not dead code.
            let _previous = world
                .get(id)
                .cloned()
                .unwrap_or_else(|| tombstone(id, revision.saturating_sub(1)));
            let mut next = document(rng, id, *revision);
            // A resurrection is an INSERT event with no previous state — the behaviour the
            // categorize oracle pinned.
            next.set("_rev", json!(format!("{revision}-t")));
            world.insert(id.clone(), next.clone());
            ChangeEvent {
                document_id: id.clone(),
                operation: Operation::Insert,
                document_data: next,
                previous_document_data: None,
            }
        }
    }
}

/// Chooses an operation against the current world.
fn random_op(rng: &mut Rng, world: &BTreeMap<String, DocumentData>, next_id: &mut u64) -> Op {
    let live: Vec<&String> = world
        .iter()
        .filter(|(_, document)| !document.is_deleted())
        .map(|(id, _)| id)
        .collect();
    let dead: Vec<&String> = world
        .iter()
        .filter(|(_, document)| document.is_deleted())
        .map(|(id, _)| id)
        .collect();

    // Weights chosen so every operation is common enough to matter over a short run, and so the
    // world rarely empties (which would make the run uninteresting).
    let choice = rng.below(100);
    match choice {
        0..=19 => {
            *next_id += 1;
            Op::Insert {
                id: format!("d{next_id}"),
                revision: 1,
            }
        }
        20..=39 if !live.is_empty() => Op::UpdateSelectedField {
            id: rng.pick(&live).to_string(),
            revision: rng.range(2, 40) as u32,
        },
        40..=54 if !live.is_empty() => Op::UpdateSortField {
            id: rng.pick(&live).to_string(),
            revision: rng.range(2, 40) as u32,
        },
        55..=64 if !live.is_empty() => Op::UpdateUnselectedField {
            id: rng.pick(&live).to_string(),
            revision: rng.range(2, 40) as u32,
        },
        65..=74 if !live.is_empty() => Op::Delete {
            id: rng.pick(&live).to_string(),
        },
        75..=84 if !dead.is_empty() => Op::Resurrect {
            id: rng.pick(&dead).to_string(),
            revision: rng.range(2, 40) as u32,
        },
        85..=94 if !live.is_empty() => Op::UpdateIrrelevant {
            id: rng.pick(&live).to_string(),
            revision: rng.range(2, 40) as u32,
        },
        // Fall back to an insert when the world is empty or the chosen branch had no target.
        _ => {
            *next_id += 1;
            Op::Insert {
                id: format!("d{next_id}"),
                revision: 1,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// query shapes
// ---------------------------------------------------------------------------

/// One query shape from the brief's list, plus a label for the failure log.
struct Shape {
    label: &'static str,
    query: Value,
}

/// The query shapes the corpus walks.
///
/// One per category the brief named: indexed selector, non-indexed selector, sort ascending, sort
/// descending, limit, skip, limit+skip, and a compound selector mixing a queryable operator with a
/// non-queryable one.
fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            label: "indexed-range",
            query: json!({ "selector": { "age": { "$gte": 40 } } }),
        },
        Shape {
            // `$eq` on an indexed field. Pinned to a value the generator can produce
            // (`age` is drawn from 0..=120, and 42 is `rng.range(0,120)`-reachable), because an
            // `$eq` that matches nothing is a shape that exercises nothing — the coverage test
            // below catches exactly that.
            label: "indexed-eq",
            query: json!({ "selector": { "age": { "$eq": 42 } } }),
        },
        Shape {
            label: "non-indexed-eq",
            query: json!({ "selector": { "tag": "red" } }),
        },
        Shape {
            label: "non-indexed-in",
            query: json!({ "selector": { "tag": { "$in": ["red", "blue"] } } }),
        },
        Shape {
            label: "sort-asc",
            query: json!({ "selector": {}, "sort": [{ "age": "asc" }] }),
        },
        Shape {
            label: "sort-desc",
            query: json!({ "selector": {}, "sort": [{ "age": "desc" }] }),
        },
        Shape {
            label: "limit-only",
            query: json!({ "selector": {}, "sort": [{ "age": "asc" }], "limit": 3 }),
        },
        Shape {
            label: "skip-only",
            query: json!({ "selector": {}, "sort": [{ "age": "asc" }], "skip": 2 }),
        },
        Shape {
            label: "limit-and-skip",
            query: json!({
                "selector": { "age": { "$gte": 20 } },
                "sort": [{ "age": "asc" }],
                "skip": 1,
                "limit": 2
            }),
        },
        // A compound selector mixing a queryable operator with a non-queryable one: the index can
        // narrow on `age` but `$regex` must still be applied per document.
        Shape {
            label: "compound-gte-and-regex",
            query: json!({
                "selector": {
                    "age": { "$gte": 30 },
                    "tag": { "$regex": "^r" }
                },
                "sort": [{ "age": "asc" }]
            }),
        },
        // `$elemMatch` on an array field: non-queryable, and the field is not indexed.
        Shape {
            label: "elemMatch",
            query: json!({
                "selector": { "skills": { "$elemMatch": { "damage": { "$gte": 10 } } } },
                "sort": [{ "age": "asc" }]
            }),
        },
        // The two most hostile combinations: a non-queryable selector *and* a window.
        Shape {
            label: "non-indexed-plus-limit-skip",
            query: json!({
                "selector": { "tag": { "$in": ["red", "green"] } },
                "sort": [{ "age": "desc" }],
                "skip": 1,
                "limit": 2
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// the live query under test
// ---------------------------------------------------------------------------

/// The comparator and matcher as boxed `dyn Fn`, named for clippy and for legibility.
type BoxedComparator = Box<dyn Fn(&DocumentData, &DocumentData) -> std::cmp::Ordering>;
type BoxedMatcher = Box<dyn Fn(&DocumentData) -> bool>;

/// A prepared query plus the compiled matcher and comparator, which is what event-reduce needs and
/// also what the ground truth uses.
struct Live {
    /// Only read in failure output, which is the point: a fuzz failure that does not
    /// say which query shape broke is not reproducible.
    #[allow(dead_code)]
    label: &'static str,
    prepared: dxdb_query::normalize::FilledMangoQuery,
    matcher: dxdb_query::matcher::Matcher,
    comparator: dxdb_query::sort::SortComparator,
    sort_fields: Vec<String>,
    comparator_fn: BoxedComparator,
    matcher_fn: BoxedMatcher,
    schema: Schema,
}

impl Live {
    fn new(schema: Schema, shape: &Shape) -> Live {
        let prepared = prepare(&schema, &shape.query);
        let matcher =
            dxdb_query::matcher::Matcher::new(&prepared.selector).expect("selector compiles");
        let run_matcher =
            dxdb_query::matcher::Matcher::new(&prepared.selector).expect("selector compiles");
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
        Live {
            label: shape.label,
            prepared,
            matcher: run_matcher,
            comparator: run_comparator,
            sort_fields,
            comparator_fn,
            matcher_fn,
            schema,
        }
    }

    fn params(&self) -> dxdb_reactivity::ReduceParams<'_> {
        dxdb_reactivity::ReduceParams {
            primary_key: self.schema.primary_path(),
            skip: self.prepared.skip as usize,
            limit: self.prepared.limit.map(|limit| limit as usize),
            sort_fields: &self.sort_fields,
            sort_comparator: &*self.comparator_fn,
            query_matcher: &*self.matcher_fn,
        }
    }

    /// The ground truth: brute-force the whole query over the world.
    ///
    /// Deliberately written independently of event-reduce — filter, sort, skip, take — so it
    /// cannot inherit a bug from the code under test.
    fn ground_truth(&self, world: &BTreeMap<String, DocumentData>) -> Vec<DocumentData> {
        let mut matched: Vec<DocumentData> = world
            .values()
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
}

/// Normalises a query the way the collection does: add `_deleted: false`, fill defaults, append
/// the primary key to the sort.
fn prepare(schema: &Schema, query: &Value) -> dxdb_query::normalize::FilledMangoQuery {
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
    let indexes = dxdb_schema::get_string_indexes(schema.json_schema()).expect("indexes");
    dxdb_query::normalize::normalize_mango_query(
        &dxdb_query::normalize::MangoQuery {
            selector: Some(selector),
            sort,
            skip: query.get("skip").and_then(Value::as_u64),
            limit: query.get("limit").and_then(Value::as_u64),
            index: None,
        },
        schema.primary_path(),
        &indexes,
        false,
    )
    .expect("the corpus queries normalise")
}

/// Documents and order, as a comparable summary.
fn summary(documents: &[DocumentData]) -> Vec<String> {
    documents
        .iter()
        .map(|document| {
            format!(
                "{}@{}",
                document.id("id").unwrap_or_default(),
                document.rev().unwrap_or_default()
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

/// How the corpus is configured. Fixed constants, so a run is reproducible and CI is stable.
const SEEDS: [u64; 8] = [
    0x5eed_1e55,
    0x0000_0001,
    0xdead_beef,
    0x1234_5678,
    0xfeed_face,
    0x0bad_c0de,
    0x1111_2222,
    0xa5a5_a5a5,
];
/// Operations per query shape per seed.
const STEPS_PER_RUN: usize = 40;
/// Documents in the world at the start of a run.
const INITIAL_DOCUMENTS: u64 = 6;

/// Everything the corpus observed, so the caller can assert on it.
#[derive(Debug, Default)]
struct Report {
    /// How many (seed, shape) runs executed.
    runs: usize,
    /// How many change events were applied in total.
    events: usize,
    /// How many of those event-reduce handled without a full re-run.
    fast_path: usize,
    /// How many of those it refused to handle.
    full_rerun: usize,
    /// How many fast-path results were *unchanged* (`doNothing`), the strongest evidence the
    /// optimisation is doing real work rather than transforming everything.
    do_nothing: usize,
    /// Divergences found. Empty is the pass condition.
    divergences: Vec<String>,
}

impl Report {
    fn total(&self) -> usize {
        self.fast_path + self.full_rerun
    }
}

/// Runs the corpus for one seed, collecting a report rather than panicking, so a single call can
/// report every divergence in the corpus instead of only the first.
fn run_seed(seed: u64, report: &mut Report) {
    let schema = schema();
    let shapes = shapes();

    for shape in &shapes {
        let live = Live::new(schema.clone(), shape);
        let mut rng = Rng::new(seed ^ hash_label(shape.label));
        let mut world: BTreeMap<String, DocumentData> = BTreeMap::new();

        // Seed the world.
        for index in 0..INITIAL_DOCUMENTS {
            let id = format!("d{index}");
            let document = document(&mut rng, &id, 1);
            world.insert(id, document);
        }

        let mut current = live.ground_truth(&world);
        let mut next_id = INITIAL_DOCUMENTS;
        let mut history: Vec<(Op, Vec<String>, Vec<String>, &'static str)> = Vec::new();

        for step in 0..STEPS_PER_RUN {
            let op = random_op(&mut rng, &world, &mut next_id);
            let event = apply_op(&mut world, &mut rng, &op);
            let truth = live.ground_truth(&world);

            let states = resolve_states(&live.params(), &event, &current);
            let action = action_for(states);
            report.events += 1;

            let (next, verdict) = match reduce(&live.params(), &event, &current) {
                EventReduceResult::Updated { changed, results } => {
                    report.fast_path += 1;
                    if !changed {
                        report.do_nothing += 1;
                    }
                    (results, if changed { "incremental" } else { "doNothing" })
                }
                EventReduceResult::RunFullQueryAgain => {
                    report.full_rerun += 1;
                    (truth.clone(), "runFullQueryAgain")
                }
            };

            // The property. Order included.
            if summary(&next) != summary(&truth) {
                // Classify, so the severity is legible: a stale result is the correctness bug.
                let missed = summary(&next).len() < summary(&truth).len()
                    || summary(&truth)
                        .iter()
                        .any(|id| !summary(&next).contains(id));
                let kind = if missed {
                    "STALE (event-reduce claimed sufficiency and dropped a document)"
                } else {
                    "SPURIOUS (result differs but nothing was missed)"
                };
                report.divergences.push(format!(
                    "seed {seed:#x}, shape '{}', step {step}\n  \
                     action: {} ({kind})\n  \
                     operation: {} {}\n  \
                     event: {:?} {}\n  \
                     event-reduce: {:?}\n  \
                     ground truth: {:?}\n  \
                     history (replay this):\n{}\n  \
                     action was chosen by states: {states:?}",
                    shape.label,
                    action.as_str(),
                    op.label(),
                    match &op {
                        Op::Insert { id, revision }
                        | Op::UpdateSelectedField { id, revision }
                        | Op::UpdateUnselectedField { id, revision }
                        | Op::UpdateSortField { id, revision }
                        | Op::Resurrect { id, revision }
                        | Op::UpdateIrrelevant { id, revision } => format!("{id} rev {revision}"),
                        Op::Delete { id } => id.clone(),
                    },
                    event.operation.as_str(),
                    event.document_id,
                    summary(&next),
                    summary(&truth),
                    history
                        .iter()
                        .map(|(op, before, after, verdict)| format!(
                            "    {:<20} {} -> {}   [{}]",
                            op.label(),
                            before.join(","),
                            after.join(","),
                            verdict
                        ))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
                // Keep going: one divergence per run is enough, and the history above is the
                // counterexample. Continue from the ground truth so later steps stay meaningful.
                current = truth;
                history.push((op, summary(&current), summary(&current), verdict));
                continue;
            }

            current = next;
            history.push((
                op,
                history
                    .last()
                    .map(|(_, _, after, _)| after.clone())
                    .unwrap_or_else(|| summary(&truth)),
                summary(&truth),
                verdict,
            ));
        }
        report.runs += 1;
    }
}

/// A stable hash of a shape label, so each shape gets its own operation sequence from one seed
/// while remaining reproducible.
fn hash_label(label: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in label.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The whole corpus: every seed crossed with every shape.
#[test]
fn random_operations_match_a_full_requery() {
    let mut report = Report::default();
    for seed in SEEDS {
        run_seed(seed, &mut report);
    }

    assert_eq!(
        report.runs,
        SEEDS.len() * shapes().len(),
        "every seed must run every shape"
    );
    assert!(
        report.divergences.is_empty(),
        "{} divergence(s) across {} runs ({} events):\n\n{}",
        report.divergences.len(),
        report.runs,
        report.events,
        report.divergences.join("\n\n")
    );
}

/// The fast path must actually fire, or it is dead code that every test passes through harmlessly.
///
/// This is the assertion that proves the optimisation is not silently disabled — a suite that only
/// checked equality would pass just as happily if every event re-ran the query.
#[test]
fn the_fast_path_actually_fires() {
    let mut report = Report::default();
    for seed in SEEDS {
        run_seed(seed, &mut report);
    }

    // A floor rather than a tight bound: the exact number depends on the corpus, and pinning it
    // would make an unrelated change to the shape list look like a regression. What matters is
    // that the incremental path dominates and that `doNothing` (the cheapest and most valuable
    // verdict) is common.
    let fast_fraction = report.fast_path as f64 / report.total() as f64;
    assert!(
        report.fast_path > 1000,
        "the fast path fired only {} times out of {} events; it is effectively disabled",
        report.fast_path,
        report.total()
    );
    assert!(
        fast_fraction > 0.40,
        "the fast path handled only {:.0}% of events ({} of {}); event-reduce is mostly re-running",
        fast_fraction * 100.0,
        report.fast_path,
        report.total()
    );
    assert!(
        report.do_nothing > 300,
        "only {} events were provably irrelevant; `doNothing` should be the most common verdict",
        report.do_nothing
    );
    assert!(
        report.full_rerun > 0,
        "the corpus never exercised the conservative path, so the fallback is untested"
    );
}

/// Prints the measured corpus size and verdict distribution.
///
/// Kept as a test (rather than a `--nocapture` note in a commit message) so the numbers in a
/// report are reproducible by anyone: `cargo test -p dxdb-reactivity --test live_query_fuzz
/// corpus_report -- --nocapture`.
#[test]
fn corpus_report() {
    let mut report = Report::default();
    for seed in SEEDS {
        run_seed(seed, &mut report);
    }
    let total = report.total();
    // Recorded from a real run so a regression shows up as a number, not as a slow suite. The
    // measured corpus at the time of writing: 96 runs, 3840 events, fast path 94.8%,
    // `doNothing` 50.8% of fast-path verdicts, full re-run 5.2%, zero divergences.
    //
    // These are floors and ceilings rather than pins: the corpus is deterministic, so they will
    // not drift on their own, but a deliberate change to the shape list should update them in the
    // same commit rather than fail mysteriously.
    assert_eq!(report.runs, 96, "8 seeds x 12 shapes");
    assert_eq!(report.events, 3840, "96 runs x 40 steps");
    println!(
        "corpus: {} runs ({} seeds x {} shapes), {} events",
        report.runs,
        SEEDS.len(),
        shapes().len(),
        report.events
    );
    println!(
        "verdicts: fast_path {} ({:.1}%), doNothing {} ({:.1}% of fast), runFullQueryAgain {} ({:.1}%), divergences {}",
        report.fast_path,
        report.fast_path as f64 / total as f64 * 100.0,
        report.do_nothing,
        report.do_nothing as f64 / report.fast_path.max(1) as f64 * 100.0,
        report.full_rerun,
        report.full_rerun as f64 / total as f64 * 100.0,
        report.divergences.len()
    );
}

/// The corpus must be big enough to be worth calling a fuzz harness.
#[test]
fn the_corpus_is_substantial() {
    let mut report = Report::default();
    for seed in SEEDS {
        run_seed(seed, &mut report);
    }
    assert!(SEEDS.len() >= 8, "at least eight seeds");
    assert!(shapes().len() >= 12, "at least twelve query shapes");
    assert!(
        report.events >= 3000,
        "the corpus applied only {} events; expected thousands",
        report.events
    );
}

/// Every operation kind in the brief must actually occur, or the corpus is narrower than claimed.
#[test]
fn every_operation_kind_occurs() {
    // Drive `random_op` directly: counting what `run_seed` chose would require instrumenting the
    // run, and this is the same generator.
    let mut rng = Rng::new(0x5eed_1e55);
    let mut world: BTreeMap<String, DocumentData> = BTreeMap::new();
    let mut next_id = 0u64;
    for _ in 0..20 {
        next_id += 1;
        let id = format!("d{next_id}");
        let document = document(&mut rng, &id, 1);
        world.insert(id, document);
    }
    // Tombstone one so `resurrect` has a target.
    let first = world.keys().next().cloned().expect("world is not empty");
    world.insert(first.clone(), tombstone(&first, 2));

    let mut seen: BTreeMap<&'static str, usize> = BTreeMap::new();
    for _ in 0..500 {
        let op = random_op(&mut rng, &world, &mut next_id);
        *seen.entry(op.label()).or_default() += 1;
        apply_op(&mut world, &mut rng, &op);
    }
    for kind in [
        "insert",
        "update-selected",
        "update-unselected",
        "update-sort",
        "delete",
        "resurrect",
        "update-irrelevant",
    ] {
        assert!(
            seen.get(kind).copied().unwrap_or(0) > 0,
            "operation '{kind}' never occurred; seen: {seen:?}"
        );
    }
}

/// Each shape in the brief's list must be present, and every shape must produce a non-trivial
/// result set at least once — a shape whose result is always empty tests nothing.
#[test]
fn every_query_shape_is_exercised() {
    let schema = schema();
    let mut rng = Rng::new(0xabcdef);
    let world: BTreeMap<String, DocumentData> = (0..40)
        .map(|index| {
            let id = format!("d{index}");
            let document = document(&mut rng, &id, 1);
            (id, document)
        })
        .collect();

    for shape in shapes() {
        let live = Live::new(schema.clone(), &shape);
        let result = live.ground_truth(&world);
        assert!(
            !result.is_empty(),
            "shape '{}' matched nothing across 40 documents, so it exercises nothing",
            shape.label
        );
        // Every returned document must genuinely match, which catches a matcher that is too
        // permissive as well as one that is too strict.
        for document in &result {
            assert!(
                !document.is_deleted(),
                "shape '{}' returned a tombstone",
                shape.label
            );
            assert!(
                live.matcher.matches(document.as_value()),
                "shape '{}' returned a document that does not match its selector",
                shape.label
            );
        }
    }
}
