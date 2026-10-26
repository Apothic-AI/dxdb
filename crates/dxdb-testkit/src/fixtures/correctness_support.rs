//! The types the generated correctness data is expressed in.
//!
//! Kept separate from the generated module so the generated file is pure data and
//! this file is pure shape: a reviewer diffing generated output sees only values
//! change.

use serde_json::{Map, Value};

/// Where a group's schema comes from.
///
/// Upstream writes three shapes, and the difference is load-bearing:
///
/// * [`SchemaSource::Named`] — the fixture exactly as `schemas.ts` defines it;
/// * [`SchemaSource::WithIndexes`] — upstream's `withIndexes(schema, indexes)`,
///   which **clones** the fixture, replaces `indexes`, and appends any indexed
///   field that is not already `required`. The clone matters: the fixture object is
///   shared between groups, so mutating it in place would change an earlier group.
///   It is modelled as an operation rather than as 25 pre-expanded schemas so the
///   port cannot accidentally share the mutation;
/// * [`SchemaSource::Inline`] — a schema written out inside the group.
#[derive(Clone)]
pub enum SchemaSource {
    /// A fixture from [`crate::schemas`].
    Named(fn() -> Value),
    /// `withIndexes(base, indexes)`.
    WithIndexes {
        /// The fixture to clone.
        base: fn() -> Value,
        /// The fixture's name, for an error message that names upstream's symbol.
        base_name: &'static str,
        /// Replacement `indexes`, as JSON (a list of strings and/or string lists).
        ///
        /// Owned rather than `&'static [Value]` because each entry is a `json!(..)`
        /// macro call, whose value is built at runtime; the group it belongs to is
        /// itself cached in a `OnceLock`, so it is constructed exactly once.
        indexes: Vec<Value>,
    },
    /// A schema literal written inside the group.
    Inline(&'static str),
}

impl std::fmt::Debug for SchemaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaSource::Named(_) => f.write_str("Named(..)"),
            SchemaSource::WithIndexes { base_name, .. } => {
                write!(f, "WithIndexes({base_name}, ..)")
            }
            SchemaSource::Inline(_) => f.write_str("Inline(..)"),
        }
    }
}

impl SchemaSource {
    /// Materialises the schema this source describes.
    ///
    /// [`SchemaSource::WithIndexes`] reproduces upstream's `withIndexes`
    /// (`rx-storage-query-correctness.test.ts:89`) exactly:
    ///
    /// ```text
    /// schema = clone(schema);
    /// schema.indexes = indexes;
    /// indexFields.forEach(f => { if (!required.includes(f)) required.push(f); });
    /// ```
    ///
    /// The `required` append is the part that is easy to drop and then not notice:
    /// a required field cannot be missing, so a group whose data relies on a
    /// *missing* indexed value stops testing what it meant to test.
    pub fn materialise(&self) -> Value {
        match self {
            SchemaSource::Named(make) => make(),
            SchemaSource::Inline(text) => serde_json::from_str(text)
                .unwrap_or_else(|err| panic!("inline schema is malformed ({err}): {text}")),
            SchemaSource::WithIndexes {
                base,
                base_name,
                indexes,
            } => {
                let mut schema = base();
                let object = schema
                    .as_object_mut()
                    .unwrap_or_else(|| panic!("withIndexes base {base_name} is not a JSON object"));
                object.insert("indexes".to_string(), Value::Array(indexes.to_vec()));

                // `indexFields = indexes.flat()`
                let mut index_fields: Vec<String> = Vec::new();
                for index in indexes.iter() {
                    match index {
                        Value::String(field) => index_fields.push(field.clone()),
                        Value::Array(parts) => {
                            for part in parts {
                                if let Some(field) = part.as_str() {
                                    index_fields.push(field.to_string());
                                }
                            }
                        }
                        other => panic!(
                            "withIndexes base {base_name}: index entry is neither a string nor an array: {other}"
                        ),
                    }
                }

                let required = object
                    .entry("required".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                let list = required.as_array_mut().unwrap_or_else(|| {
                    panic!("withIndexes base {base_name}: `required` is not an array")
                });
                for field in index_fields {
                    let already = list
                        .iter()
                        .any(|item| item.as_str() == Some(field.as_str()));
                    if !already {
                        list.push(Value::String(field));
                    }
                }
                schema
            }
        }
    }
}

/// One query in a correctness group.
#[derive(Clone, Debug)]
pub struct Query {
    /// Upstream's `info`, used verbatim in a failure message. The whole point of
    /// the field is that a failure names the case in upstream's words.
    pub info: &'static str,
    /// The Mango query, as the JSON upstream wrote.
    pub query: Value,
    /// The document ids `query()` must return, in order.
    pub expected_result_doc_ids: &'static [&'static str],
    /// Upstream's assertion on the planner's `selectorSatisfiedByIndex`, when it
    /// makes one.
    ///
    /// `None` means upstream asserts nothing about it, which is different from
    /// `Some(false)`.
    pub selector_satisfied_by_index: Option<bool>,
}

impl Query {
    /// The selector, which is always an object after normalisation. Upstream
    /// leaves `selector` off three queries, so this returns `{}` for those, the
    /// same way `normalizeMangoQuery` would.
    pub fn selector(&self) -> &Map<String, Value> {
        self.query
            .get("selector")
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
                EMPTY.get_or_init(Map::new)
            })
    }

    /// The sort parts, as `(field, direction)` in order. Upstream omits `sort` on
    /// some queries, in which case this is empty and the storage is free to use
    /// any order — which is exactly what the case then does *not* assert, so an
    /// empty list is meaningful.
    pub fn sort(&self) -> Vec<(String, String)> {
        let Some(sort) = self.query.get("sort").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for part in sort {
            let Some(object) = part.as_object() else {
                continue;
            };
            for (field, direction) in object {
                out.push((
                    field.clone(),
                    direction.as_str().unwrap_or("asc").to_string(),
                ));
            }
        }
        out
    }

    /// `skip`, defaulting to 0 like `normalizeMangoQuery` does.
    pub fn skip(&self) -> usize {
        self.query.get("skip").and_then(Value::as_u64).unwrap_or(0) as usize
    }

    /// `limit`, or `None` when upstream wrote none.
    pub fn limit(&self) -> Option<usize> {
        self.query
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
    }

    /// The forced `index`, when upstream names one.
    pub fn index(&self) -> Option<Vec<String>> {
        let index = self.query.get("index")?.as_array()?;
        Some(
            index
                .iter()
                .filter_map(|part| part.as_str().map(str::to_string))
                .collect(),
        )
    }

    /// True when this query has neither `skip` nor `limit`, which is the condition
    /// upstream uses to decide whether to also assert `.count()`
    /// (`rx-storage-query-correctness.test.ts:296`).
    pub fn asserts_count(&self) -> bool {
        self.limit().is_none() && self.query.get("skip").is_none()
    }
}

/// One upstream `testCorrectQueries` group.
#[derive(Clone, Debug)]
pub struct Group {
    /// 1-based index, matching [`crate::decl::QUERY_CORRECTNESS_CASES`] order.
    pub index: usize,
    /// Upstream's `testTitle`, which is also the `it()` title.
    pub title: &'static str,
    /// 1-based line of the `testTitle` in the upstream file.
    pub upstream_line: u32,
    /// The primary key path of this group's schema.
    pub primary_path: &'static str,
    /// Where the schema comes from.
    pub schema: SchemaSource,
    /// The documents upstream inserts, one per entry.
    pub data: Vec<Value>,
    /// The queries and their expected results.
    pub queries: Vec<Query>,
    /// True for the one group upstream skips on dexie storages.
    pub not_run_if_dexie: bool,
    /// The upstream fixture calls that produced `data`, one entry per document,
    /// empty where upstream wrote the literal out. Kept so the committed data is
    /// traceable to the expression that generated it.
    pub recipes: &'static [&'static str],
}

impl Group {
    /// The materialised schema, with `withIndexes` applied.
    pub fn materialised_schema(&self) -> Value {
        self.schema.materialise()
    }

    /// The document id of one entry of [`Group::data`].
    pub fn id_of<'a>(&self, document: &'a Value) -> Option<&'a str> {
        document.get(self.primary_path).and_then(Value::as_str)
    }

    /// The number of queries in this group.
    pub fn query_count(&self) -> usize {
        self.queries.len()
    }
}

/// Total queries across every group, as a function so a test asserts it against
/// the declaration table rather than against a number in prose.
pub fn query_count(groups: &[Group]) -> usize {
    groups.iter().map(Group::query_count).sum()
}

/// Total documents across every group.
pub fn document_count(groups: &[Group]) -> usize {
    groups.iter().map(|group| group.data.len()).sum()
}
