//! The query cache: one live query object per distinct normalised query.
//!
//! Ported from `src/query-cache.ts` (140 lines) and the `toString()` that keys it
//! (`src/rx-query.ts:454`).
//!
//! # What it is for
//!
//! Two components calling `collection.find({...})` with the same query must share one live
//! query, so the storage runs the query once and both subscribers see the same events. Without
//! that, N copies of a query mean N full scans and N sets of subscriptions.
//!
//! # The key, and the trap in it
//!
//! The key is the **normalised** query serialised with **sorted object keys**, not the query the
//! caller wrote. That is what makes `{selector: {a: 1, b: 2}}` and `{selector: {b: 2, a: 1}}`
//! the same query — they normalise and then sort to identical text.
//!
//! Two consequences the port has to keep:
//!
//! * **`findByIds` keys on the id list, and the order matters.** Upstream's comment is explicit:
//!   the ids are not sorted because that is expensive, and the id order defines the result
//!   `Map`'s iteration order, so two id lists that differ only in order must *not* share a key.
//!   This is the one query shape whose key is not order-insensitive.
//! * **`sort` is part of the key.** A query with `sort: [{age: 'asc'}]` and one with
//!   `sort: [{age: 'desc'}]` produce different results and must not collide.
//!
//! The serialisation goes through `dxdb_json`'s JavaScript number formatting, because two
//! queries differing only in how a number was written (`1` vs `1.0`) are the same query and must
//! produce the same key.

use std::collections::HashMap;

/// The operation a live query performs. Part of the cache key: `find` and `count` over one
/// query are different cache entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueryOp {
    /// `find()`
    Find,
    /// `findOne()`
    FindOne,
    /// `count()`
    Count,
    /// `findByIds()`
    FindByIds,
}

impl QueryOp {
    /// The upstream spelling, which is part of the cache key.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryOp::Find => "find",
            QueryOp::FindOne => "findOne",
            QueryOp::Count => "count",
            QueryOp::FindByIds => "findByIds",
        }
    }
}

/// Builds the cache key for a query.
///
/// `normalised_query` must be the query *after* normalisation (defaults filled, sort ending in
/// the primary key, selector shorthands expanded), because that is what upstream keys on. For
/// `FindByIds`, `ids` is the id list and the normalised query is ignored.
pub fn cache_key(
    op: QueryOp,
    normalised_query: &serde_json::Value,
    ids: Option<&[String]>,
) -> String {
    if op == QueryOp::FindByIds {
        // "|findByIds|" + JSON.stringify(ids). The order is deliberately preserved.
        let ids = ids.unwrap_or(&[]);
        let mut text = String::from("|findByIds|[");
        for (position, id) in ids.iter().enumerate() {
            if position > 0 {
                text.push(',');
            }
            text.push_str(&serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string()));
        }
        text.push(']');
        return text;
    }

    // `sortObject({ op, query, other }, true)`: recursively sort object keys, then serialise.
    let mut wrapper = serde_json::Map::new();
    wrapper.insert(
        "op".to_string(),
        serde_json::Value::String(op.as_str().to_string()),
    );
    wrapper.insert("query".to_string(), normalised_query.clone());
    // `other` holds plugin-specific extras (a `findOne` by id, for instance). Absent here.
    wrapper.insert(
        "other".to_string(),
        serde_json::Value::Object(Default::default()),
    );
    let sorted = sort_object(&serde_json::Value::Object(wrapper));
    // JavaScript number formatting, so `1` and `1.0` cannot produce different keys.
    dxdb_json::to_js_json(&sorted)
}

/// Recursively sorts object keys, leaving arrays in order.
///
/// Upstream's `sortObject` (`src/plugins/utils/utils-object.ts`). Arrays are **not** sorted:
/// `$in: ['a','b']` and `$in: ['b','a']` differ in the result's iteration order, and upstream's
/// `$in` keeps the selector's order.
fn sort_object(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // Code-unit order, not Rust byte order: this key is compared against keys built by
            // other code paths in the same process, and PLAN.md 3.2 makes code-unit the
            // workspace-wide string order.
            keys.sort_by(|left, right| dxdb_jsstr::compare_js(left, right));
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_object(&map[key]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_object).collect())
        }
        other => other.clone(),
    }
}

/// A live query, as the cache holds it.
///
/// Generic over the query object a caller stores, because the cache's job is identity rather
/// than contents: it hands back the same value for the same key.
pub struct QueryCache<T> {
    entries: HashMap<String, T>,
}

impl<T> Default for QueryCache<T> {
    fn default() -> Self {
        QueryCache {
            entries: HashMap::new(),
        }
    }
}

impl<T> QueryCache<T> {
    /// An empty cache.
    pub fn new() -> Self {
        QueryCache::default()
    }

    /// Returns the cached value for `key`, inserting `create()`'s result when absent.
    ///
    /// This is `getFromMapOrCreate`: the create closure runs only on a miss, which is what keeps
    /// the storage from being queried twice for one distinct query.
    pub fn get_or_create(&mut self, key: String, create: impl FnOnce() -> T) -> &mut T {
        self.entries.entry(key).or_insert_with(create)
    }

    /// Looks a key up without creating.
    pub fn get(&self, key: &str) -> Option<&T> {
        self.entries.get(key)
    }

    /// Removes a key, which `uncacheRxQuery` does when a query is explicitly uncached.
    pub fn remove(&mut self, key: &str) -> Option<T> {
        self.entries.remove(key)
    }

    /// How many queries are cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every key, sorted, so a diagnostic is deterministic.
    pub fn keys(&self) -> Vec<&String> {
        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort_by(|left, right| dxdb_jsstr::compare_js(left, right));
        keys
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Upstream's default cap on cached queries before a replacement pass runs.
pub const DEFAULT_TRY_TO_KEEP_MAX: usize = 100;
/// Upstream's default lifetime, in milliseconds, for an unexecuted query.
pub const DEFAULT_UNEXECUTED_LIFETIME: u64 = 30 * 1000;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Key order must not matter: two spellings of one query share a cache entry.
    #[test]
    fn object_key_order_does_not_change_the_key() {
        let left = cache_key(
            QueryOp::Find,
            &json!({ "selector": { "a": 1, "b": 2 }, "sort": [{ "a": "asc" }] }),
            None,
        );
        let right = cache_key(
            QueryOp::Find,
            &json!({ "selector": { "b": 2, "a": 1 }, "sort": [{ "a": "asc" }] }),
            None,
        );
        assert_eq!(left, right, "normalised key order must not split the cache");
    }

    /// The operation is part of the key: `find` and `count` are different entries.
    #[test]
    fn the_operation_is_part_of_the_key() {
        let query = json!({ "selector": {}, "sort": [{ "id": "asc" }] });
        assert_ne!(
            cache_key(QueryOp::Find, &query, None),
            cache_key(QueryOp::Count, &query, None)
        );
        assert_ne!(
            cache_key(QueryOp::Find, &query, None),
            cache_key(QueryOp::FindOne, &query, None)
        );
    }

    /// Sort direction is part of the key, because it changes the result order.
    #[test]
    fn sort_direction_is_part_of_the_key() {
        let ascending = cache_key(
            QueryOp::Find,
            &json!({ "selector": {}, "sort": [{ "age": "asc" }] }),
            None,
        );
        let descending = cache_key(
            QueryOp::Find,
            &json!({ "selector": {}, "sort": [{ "age": "desc" }] }),
            None,
        );
        assert_ne!(ascending, descending);
    }

    /// Array order is *not* sorted away: `$in` order defines the result iteration order, so two
    /// id lists differing only in order must not collide.
    #[test]
    fn find_by_ids_keys_on_the_id_order() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let reversed = vec!["b".to_string(), "a".to_string()];
        let query = json!({});
        let forward = cache_key(QueryOp::FindByIds, &query, Some(&ids));
        let backward = cache_key(QueryOp::FindByIds, &query, Some(&reversed));
        assert_ne!(
            forward, backward,
            "the id order defines the result Map's order, so these are different queries"
        );
        assert!(forward.starts_with("|findByIds|["), "key shape: {forward}");

        // And the same order produces the same key.
        assert_eq!(forward, cache_key(QueryOp::FindByIds, &query, Some(&ids)));
    }

    /// Numbers are keyed by their JavaScript formatting, so `1` and `1.0` are one query.
    #[test]
    fn number_formatting_does_not_split_the_cache() {
        let integer = cache_key(
            QueryOp::Find,
            &json!({ "selector": { "age": { "$eq": 1 } } }),
            None,
        );
        let float = cache_key(
            QueryOp::Find,
            &json!({ "selector": { "age": { "$eq": 1.0 } } }),
            None,
        );
        assert_eq!(integer, float);
    }

    /// `get_or_create` runs the closure only on a miss, which is what stops a duplicate storage
    /// query.
    #[test]
    fn get_or_create_only_creates_on_a_miss() {
        let mut cache: QueryCache<u32> = QueryCache::new();
        let mut created = 0;
        let first = *cache.get_or_create("key".to_string(), || {
            created += 1;
            7
        });
        assert_eq!(first, 7);
        let second = *cache.get_or_create("key".to_string(), || {
            created += 1;
            99
        });
        assert_eq!(second, 7, "the existing entry wins");
        assert_eq!(created, 1, "the closure must run once");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn remove_and_clear_work() {
        let mut cache: QueryCache<u32> = QueryCache::new();
        cache.get_or_create("a".to_string(), || 1);
        cache.get_or_create("b".to_string(), || 2);
        assert_eq!(cache.keys(), vec!["a", "b"]);
        assert_eq!(cache.remove("a"), Some(1));
        assert!(cache.get("a").is_none());
        cache.clear();
        assert!(cache.is_empty());
    }
}
