//! The k-way merge: how N shard results become one unsharded-looking result.
//!
//! This is the file `specs/sharding.md` section 3.1 is about. Three things decide
//! whether the merge is exact or merely close:
//!
//! 1. **The comparator must be the query's own.** Not `str::cmp`, not a JSON compare:
//!    the port's order is UTF-16 code-unit order on strings (`dxdb_jsstr::compare_js`,
//!    PLAN.md 3.2) with the type-precedence and missing-field rules in
//!    `dxdb_query::sort`. A merge that compared differently would interleave results
//!    differently for any corpus with astral-plane characters in a sorted field —
//!    silently, and only for those users. So the comparator is built from the same
//!    `FilledMangoQuery` the shards were given.
//! 2. **`skip` and `limit` are applied after the merge, never before.** Asking each
//!    shard for `limit` and merging is wrong: the first `limit` documents overall may
//!    all come from one shard.
//! 3. **Stability.** Two documents that compare equal must come out in the same
//!    relative order as an unsharded sort would leave them. The unsharded storage uses
//!    a stable sort, so a tie is broken by the order the documents were visited. Under
//!    sharding the shards visit them in shard order, which is not the unsharded order
//!    — so a tie must be broken explicitly, by primary key, matching the tiebreak the
//!    normalised query already carries.

use std::cmp::Ordering;

use dxdb_query::normalize::FilledMangoQuery;
use dxdb_query::{get_sort_comparator, SortComparator};
use dxdb_storage::{DocumentData, MangoQuery, PreparedQuery};

/// Compares two shard results the way the query says to.
///
/// Wraps `dxdb_query`'s comparator rather than reimplementing it, so there is exactly
/// one definition of the ordering in the workspace.
pub struct MergeOrder {
    comparator: SortComparator,
    primary_path: String,
}

impl MergeOrder {
    /// Builds the ordering for `query`.
    pub fn new(query: &FilledMangoQuery, primary_path: &str) -> Self {
        Self {
            comparator: get_sort_comparator(query),
            primary_path: primary_path.to_string(),
        }
    }

    /// Builds the ordering for a prepared query.
    ///
    /// The storage ABI carries its own `MangoQuery`; it is lifted into the query
    /// crate's type so the comparator is the one every other consumer uses.
    pub fn for_prepared(prepared: &PreparedQuery, primary_path: &str) -> Self {
        Self::new(&lift_query(&prepared.query), primary_path)
    }

    /// The ordering a change feed uses: `(_meta.lwt, primary)`.
    ///
    /// The feed is defined in that order regardless of the caller's query, so the merge
    /// for `get_changed_documents_since` cannot reuse a query's comparator.
    pub fn for_change_feed(primary_path: &str) -> Self {
        Self {
            comparator: get_sort_comparator(&FilledMangoQuery {
                selector: serde_json::json!({}),
                sort: vec![
                    dxdb_query::normalize::SortPart {
                        field: "_meta.lwt".to_string(),
                        direction: dxdb_query::normalize::SortDirection::Asc,
                    },
                    dxdb_query::normalize::SortPart {
                        field: primary_path.to_string(),
                        direction: dxdb_query::normalize::SortDirection::Asc,
                    },
                ],
                skip: 0,
                limit: None,
                index: None,
            }),
            primary_path: primary_path.to_string(),
        }
    }

    /// Orders two documents.
    ///
    /// The sort parts decide first. When they tie — which they can, because a sort is
    /// not required to be unique — the primary key breaks the tie. That tiebreak is
    /// what makes the merge's output *identical* to an unsharded result rather than
    /// merely a valid ordering of the same set: an unsharded result is also ordered by
    /// primary key within a tie, because the normalised query appends the primary key
    /// to the sort (upstream's `normalizeMangoQuery` does exactly this).
    pub fn compare(&self, left: &DocumentData, right: &DocumentData) -> Ordering {
        let by_sort = self.comparator.compare(left.as_value(), right.as_value());
        if by_sort != Ordering::Equal {
            return by_sort;
        }
        let left_id = left.id(&self.primary_path).unwrap_or_default();
        let right_id = right.id(&self.primary_path).unwrap_or_default();
        js_string_order(left_id, right_id)
    }
}

/// Code-unit comparison for the primary-key tiebreak.
///
/// `dxdb_jsstr::compare_js` is the exact function `dxdb_query`'s sort comparator uses
/// for its own string comparisons, so the tiebreak is in the same order as the sort
/// parts — which is what keeps the merged result identical to an unsharded one rather
/// than merely valid.
fn js_string_order(left: &str, right: &str) -> Ordering {
    dxdb_jsstr::compare_js(left, right)
}

/// Lifts the storage ABI's query into the query crate's filled query.
fn lift_query(query: &MangoQuery) -> FilledMangoQuery {
    FilledMangoQuery {
        selector: query.selector.clone(),
        sort: query
            .sort
            .iter()
            .map(|part| dxdb_query::normalize::SortPart {
                field: part.field.clone(),
                direction: match part.direction {
                    dxdb_storage::SortDirection::Asc => dxdb_query::normalize::SortDirection::Asc,
                    dxdb_storage::SortDirection::Desc => dxdb_query::normalize::SortDirection::Desc,
                },
            })
            .collect(),
        skip: query.skip as u64,
        limit: query.limit.map(|limit| limit as u64),
        index: query.index.clone(),
    }
}

/// How many documents to ask each shard for.
///
/// `skip + limit` when there is a limit, because in the worst case every one of the
/// global `skip` documents lives in that shard, so a shard must be prepared to supply
/// that many before its contribution to the global window can be known. When there is
/// no limit, the shard's own limit is `None`.
///
/// This over-fetch can only ever fetch *more* than a shard could contribute, so it
/// cannot change the result — it is an optimisation, and section 3.2 of the spec proves
/// why it is safe.
pub fn per_shard_limit(prepared: &PreparedQuery) -> Option<usize> {
    prepared
        .query
        .limit
        .map(|limit| prepared.query.skip.saturating_add(limit))
}

/// A prepared query with the shard-level `skip` set to zero and the limit widened.
///
/// The per-shard query keeps the same selector, sort and index; only the window moves,
/// because the global window is applied after the merge.
pub fn shard_query(prepared: &PreparedQuery, shard_limit: Option<usize>) -> PreparedQuery {
    let mut query = prepared.query.clone();
    query.skip = 0;
    query.limit = shard_limit;
    PreparedQuery {
        query,
        query_plan: prepared.query_plan.clone(),
    }
}

/// Merges already-sorted shard results into one sorted vector.
///
/// Each input must already be in the query's order, which the inner storage's contract
/// guarantees. The merge is a stable k-way merge: on a tie the earlier input wins,
/// which combined with the primary-key tiebreak in [`MergeOrder`] gives a total order.
pub fn merge_sorted(order: &MergeOrder, shards: Vec<Vec<DocumentData>>) -> Vec<DocumentData> {
    // A simple k-way merge. `shards` is small (the config's shard count) and each
    // vector is already sorted, so a cursor per shard is O(total * log N) with a small
    // constant and no allocation of a heap node per document.
    let mut cursors = vec![0usize; shards.len()];
    let total: usize = shards.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    loop {
        let mut best: Option<(usize, &DocumentData)> = None;
        for (index, shard) in shards.iter().enumerate() {
            let Some(document) = shard.get(cursors[index]) else {
                continue;
            };
            best = match best {
                None => Some((index, document)),
                Some((best_index, best_document)) => {
                    match order.compare(document, best_document) {
                        // Strictly less: this shard wins.
                        Ordering::Less => Some((index, document)),
                        // Equal: keep the earlier shard, which is the stable choice.
                        Ordering::Equal | Ordering::Greater => Some((best_index, best_document)),
                    }
                }
            };
        }
        let Some((index, document)) = best else {
            break;
        };
        out.push(document.clone());
        cursors[index] += 1;
    }
    out
}

/// Applies the global window to a merged sequence.
///
/// `skip` then `limit`, in that order, which is what the unsharded storage does.
pub fn apply_window(
    mut documents: Vec<DocumentData>,
    skip: usize,
    limit: Option<usize>,
) -> Vec<DocumentData> {
    if skip >= documents.len() {
        return Vec::new();
    }
    if skip > 0 {
        documents.drain(..skip);
    }
    if let Some(limit) = limit {
        documents.truncate(limit);
    }
    documents
}

/// The count a sharded storage must report for `prepared`, given the per-shard counts.
///
/// `count` must equal the length of the *equivalent query*, and the equivalent query
/// includes `skip` and `limit`. `dxdb-storage`'s contract is explicit: "Must equal the
/// length of the equivalent `query()` — including with a `limit`, which upstream spells
/// out: `.count({limit: 3})` must return what `.query({limit: 3}).documents.length`
/// returns."
///
/// The rule, from `specs/sharding.md` section 3.3:
///
/// * no limit, no skip: the sum;
/// * **limit: the sum clamped to `limit`, applied *after* the skip** — i.e.
///   `min(limit, max(0, sum - skip))`;
/// * skip, no limit: `max(0, sum - skip)`.
///
/// The skip-before-limit order is the part a first implementation gets wrong. Clamping
/// the sum to `limit` and ignoring `skip` answers "how many match" rather than "how many
/// the query returns", which differs for every query that skips. The differential test
/// against `dxdb-memory` caught it: 240 of 1100 combinations failed, all of them on
/// `count`, all of them queries with a `skip`.
///
/// This is exact: the equivalent query returns `min(limit, max(0, matched - skip))`
/// documents, and the sum of the per-shard matches is `matched` because each document
/// lives in exactly one shard (`INV-3`).
pub fn sharded_count(per_shard: &[usize], skip: usize, limit: Option<usize>) -> usize {
    let matched: usize = per_shard.iter().sum();
    // The window removes `skip` documents from the front, whatever the limit.
    let after_skip = matched.saturating_sub(skip);
    match limit {
        Some(limit) => after_skip.min(limit),
        None => after_skip,
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_window, per_shard_limit, sharded_count, MergeOrder};
    use dxdb_storage::{
        DocumentData, MangoQuery, PreparedQuery, QueryPlan, SortDirection, SortPart,
    };
    use serde_json::json;

    fn document(id: &str, name: &str) -> DocumentData {
        DocumentData::from_value(json!({"id": id, "name": name})).expect("valid")
    }

    fn prepared(sort_field: &str, skip: usize, limit: Option<usize>, desc: bool) -> PreparedQuery {
        let query = MangoQuery {
            selector: json!({}),
            sort: vec![SortPart {
                field: sort_field.to_string(),
                direction: if desc {
                    SortDirection::Desc
                } else {
                    SortDirection::Asc
                },
            }],
            skip,
            limit,
            index: None,
        };
        let plan = QueryPlan {
            index: vec!["_deleted".into(), sort_field.into()],
            sort_satisfied_by_index: false,
            selector_satisfied_by_index: false,
            start_keys: vec![],
            end_keys: vec![],
            inclusive_start: true,
            inclusive_end: true,
        };
        PreparedQuery::new(query, plan)
    }

    #[test]
    fn per_shard_limit_is_skip_plus_limit() {
        assert_eq!(
            per_shard_limit(&prepared("name", 0, Some(5), false)),
            Some(5)
        );
        assert_eq!(
            per_shard_limit(&prepared("name", 3, Some(5), false)),
            Some(8)
        );
        assert_eq!(per_shard_limit(&prepared("name", 3, None, false)), None);
    }

    #[test]
    fn merging_two_sorted_shards_is_sorted() {
        let query = prepared("name", 0, None, false);
        let order = MergeOrder::for_prepared(&query, "id");
        let left = vec![document("a", "alpha"), document("c", "charlie")];
        let right = vec![document("b", "bravo"), document("d", "delta")];
        let merged = super::merge_sorted(&order, vec![left, right]);
        assert_eq!(
            merged
                .iter()
                .map(|document| document.get("name").expect("name").clone())
                .collect::<Vec<_>>(),
            vec![
                json!("alpha"),
                json!("bravo"),
                json!("charlie"),
                json!("delta")
            ]
        );
    }

    #[test]
    fn a_tie_is_broken_by_primary_key_not_by_shard() {
        // Two documents with the same sort value; the merge must order them by
        // primary key regardless of which shard they came from.
        let query = prepared("name", 0, None, false);
        let order = MergeOrder::for_prepared(&query, "id");
        let shard_zero = vec![document("z", "same")];
        let shard_one = vec![document("a", "same")];
        let merged = super::merge_sorted(&order, vec![shard_zero, shard_one]);
        assert_eq!(
            merged
                .iter()
                .map(|document| document.get("id").expect("id").clone())
                .collect::<Vec<_>>(),
            vec![json!("a"), json!("z")],
            "the tiebreak is the primary key, not the shard order"
        );
    }

    #[test]
    fn descending_merges_in_descending_order() {
        let query = prepared("name", 0, None, true);
        let order = MergeOrder::for_prepared(&query, "id");
        let left = vec![document("c", "charlie"), document("a", "alpha")];
        let right = vec![document("d", "delta"), document("b", "bravo")];
        let merged = super::merge_sorted(&order, vec![left, right]);
        let names: Vec<_> = merged
            .iter()
            .map(|document| document.get("name").expect("name").clone())
            .collect();
        assert_eq!(
            names,
            vec![
                json!("delta"),
                json!("charlie"),
                json!("bravo"),
                json!("alpha")
            ]
        );
    }

    #[test]
    fn empty_shards_merge_to_nothing() {
        let query = prepared("name", 0, None, false);
        let order = MergeOrder::for_prepared(&query, "id");
        assert!(super::merge_sorted(&order, vec![vec![], vec![]]).is_empty());
        assert!(super::merge_sorted(&order, vec![]).is_empty());
    }

    #[test]
    fn the_window_applies_skip_then_limit() {
        let documents: Vec<_> = (0..10)
            .map(|index| document(&format!("id-{index}"), &format!("n{index}")))
            .collect();
        let windowed = apply_window(documents.clone(), 3, Some(2));
        assert_eq!(windowed.len(), 2);
        assert_eq!(windowed[0].get("id"), Some(&json!("id-3")));
        assert_eq!(windowed[1].get("id"), Some(&json!("id-4")));

        assert!(apply_window(documents.clone(), 20, Some(2)).is_empty());
        assert_eq!(apply_window(documents.clone(), 0, Some(0)).len(), 0);
        assert_eq!(apply_window(documents, 2, None).len(), 8);
    }

    #[test]
    fn the_count_rule_is_a_clamp_not_a_sum() {
        // Three shards each reporting the limit of 2, no skip.
        assert_eq!(sharded_count(&[2, 2, 2], 0, Some(2)), 2, "clamped, not 6");
        // No limit: the sum.
        assert_eq!(sharded_count(&[2, 2, 2], 0, None), 6);
        // Skip, no limit.
        assert_eq!(sharded_count(&[2, 2, 2], 4, None), 2);
        assert_eq!(sharded_count(&[2, 2, 2], 9, None), 0);
        // A limit larger than the total returns the total.
        assert_eq!(sharded_count(&[2, 2, 2], 0, Some(100)), 6);
        // **Skip is applied before the limit.** These are the cases the differential
        // test against dxdb-memory caught: a clamp that ignores the skip answers "how
        // many match" instead of "how many the query returns".
        assert_eq!(
            sharded_count(&[2, 2, 2], 1, Some(2)),
            2,
            "matched 6, skip 1, limit 2"
        );
        assert_eq!(
            sharded_count(&[2, 2, 2], 5, Some(2)),
            1,
            "matched 6, skip 5, limit 2"
        );
        assert_eq!(
            sharded_count(&[2, 2, 2], 6, Some(2)),
            0,
            "skip past the end"
        );
        assert_eq!(sharded_count(&[2, 2, 2], 100, Some(2)), 0);
        assert_eq!(sharded_count(&[2, 2, 2], 3, Some(100)), 3);
    }
}
