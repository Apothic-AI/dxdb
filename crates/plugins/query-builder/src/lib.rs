//! The chainable Mango query builder (`RxDBQueryBuilderPlugin`).
//!
//! Ported from `src/plugins/query-builder/` at upstream `3b763ee17`: the builder
//! is `mquery/nosql-query-builder.ts` (631 lines), the plugin wrapper is
//! `index.ts` (81 lines), and the merge helpers are `mquery/mquery-utils.ts`.
//!
//! # What ported, and what could not
//!
//! Upstream this is a *prototype plugin* (`index.ts:54`): it bolts seventeen
//! chainable methods onto `RxQuery.prototype` and rewrites the query on every
//! call. The builder itself — the object that owns a mutable query, validates
//! paths, and projects back to a `MangoQuery` — never touches `RxQuery`, so that
//! half ports cleanly and is what this crate is.
//!
//! The chainable-on-`RxQuery` half becomes a [`QueryBuilder`] the caller keeps
//! across steps. `query.where('age').gt(4)` upstream is
//! `builder.where_("age").gt(json!(4))?` here. The `?` is the only structural
//! difference: upstream throws `MQ5` when a comparison runs with no path, and the
//! builder returns [`Error::NoPath`]. What ends up in the selector is identical,
//! which is what the ported tests assert.
//!
//! # The rules that had to survive, with their sources
//!
//! * Equality is **always** `$eq`, never the bare value
//!   (`nosql-query-builder.ts:92,106`), so a chained operator coexists instead of
//!   overwriting it. Tests: `query-builder.test.ts:113,129,142`.
//! * A shorthand selector value survives chaining: `{age: 5}` then `.gt(3)` gives
//!   `{age: {$eq: 5, $gt: 3}}`, not `{age: {$gt: 3}}`
//!   (`nosql-query-builder.ts:492-505`; tests `:172` and the `null` case `:188`).
//! * Sort accepts three shapes — `[{field: dir}]`, mquery's `[[field, dir]]`, and
//!   the space-delimited `"field -other"` string (`:275-311`) — all normalised to
//!   Mango's `[{field: 'asc'|'desc'}]` at projection time (`:414`).
//! * Sort directions are validated with upstream's regex
//!   `^(?:ascending|asc|descending|desc|1|-1)$` against
//!   `String(value || 1).toLowerCase()` (`:557-566`). Anything else is `MQ7`.
//!   `$meta` sort values are stored as `{$meta: ...}` and passed through (`:546`).
//! * `merge` skips `__proto__`, `constructor` and `prototype`
//!   (`mquery-utils.ts:10`). For an existing key it **recurses when both sides are
//!   objects and overwrites otherwise** (`mquery-utils.ts:21-28`), and it runs
//!   `normalizeShorthandsBeforeMerge` first (`:357`), which is what makes
//!   `{age: 7}` merged with `{age: {$gt: 3}}` keep both conditions.
//! * `findByIds` refuses to build (`index.ts:42`, error `QU17`).
//! * An `all()` method must not exist, because `$all` is not a supported Mango
//!   operator (`query-builder.test.ts:221`).
//!
//! # Deliberate divergences
//!
//! Each is forced by Rust or is a fix; a reviewer should be able to diff
//! behaviour, not just structure.
//!
//! 1. **`maxScan`, `batchSize` and `comment` chain but never reach the output.**
//!    Upstream writes them into `options` (`:459`) and `toJSON` drops them
//!    (`:389-411`), so the values are accepted and silently discarded. Reproduced
//!    as-is because that is the observable contract, and documented because it is
//!    indistinguishable from a bug. [`QueryBuilder::dropped_attributes`] exists so
//!    a caller can see what was accepted.
//! 2. **Only plain `asc`/`desc` reach [`MangoQuery`].** `1`, `-1`, `"ascending"`
//!    and `"descending"` are accepted by `sort()` exactly as upstream accepts them
//!    (`:557`), and are normalised to `asc`/`desc` on the way out. Upstream instead
//!    hands mquery's `1`/`-1` to `mQuerySortToRxDBSort` (`:414`), which maps
//!    `1 => 'asc'` and everything else to `'desc'` — so `"bogus"` would sort
//!    *descending* upstream and is `MQ7` here. A silent wrong-direction sort is not
//!    worth reproducing.
//! 3. **A `$meta` sort is [`Error::UnsortableMeta`] at projection time.** Upstream
//!    stores `{field: {$meta: kind}}` in `options.sort` and passes it to
//!    `mQuerySortToRxDBSort`, which casts it to `'asc'`. dxdb's `SortPart` is a
//!    `(field, SortDirection)` pair with no room for `$meta`, so the part cannot be
//!    represented without lying. Erroring is the honest option; widening
//!    `SortPart` is lane D's call, not this crate's.
//! 4. **Numeric selector values are range-checked.** A Rust `f64` cannot hold the
//!    BigInt range JS silently rounds into, so [`js_number`] turns an
//!    out-of-safe-range integer into [`Error::UnrepresentableNumber`] rather than a
//!    silently rounded query.
//! 5. **A path is a `String`.** Upstream's `where` accepts a string or an object
//!    and throws `MQ1` otherwise (`:83`); Rust does that at compile time, so
//!    [`Error::InvalidPath`] only covers the JSON-object form.
//!
//! # Not implemented here
//!
//! The seventeen prototype methods on a live `RxQuery`. Those need lane D's query
//! handle to carry a `queryBuilderPath` in its `other` bag (`index.ts:13,26-34`).
//! When `dxdb-core` publishes that, an extension trait lands here that delegates to
//! this builder.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

pub use dxdb_storage::{MangoQuery, SortDirection, SortPart};

/// Errors, carrying upstream's error codes so a ported test can assert the code
/// it was written against.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `MQ5`: a comparison ran before `where()` set a path.
    #[error("MQ5: no path set before calling {method}")]
    NoPath {
        /// The comparison that needed the path.
        method: &'static str,
    },
    /// `MQ1`: `where()` was given something that is not a path or an object.
    #[error("MQ1: invalid path {path}")]
    InvalidPath {
        /// The rejected value, rendered as JSON.
        path: String,
    },
    /// `MQ4`: `merge()` was given something that is not an object.
    #[error("MQ4: cannot merge {value}")]
    MergeNotAnObject {
        /// The rejected source, rendered as JSON.
        value: String,
    },
    /// `MQ2`: `elemMatch` had neither an object nor a callback.
    #[error("MQ2: elemMatch needs an object or a query")]
    ElemMatchArguments,
    /// `MQ3`: `sort()` was given a value of an unsupported type.
    #[error("MQ3: invalid sort argument {args}")]
    SortArguments {
        /// The rejected argument, rendered as JSON.
        args: String,
    },
    /// `MQ7`: a sort direction that is not asc/desc/1/-1.
    #[error("MQ7: invalid sort direction for field {field}: {value}")]
    SortDirection {
        /// The field being sorted.
        field: String,
        /// The rejected direction.
        value: String,
    },
    /// `QU16`: a `$regex` object. Upstream refuses a `RegExp` because it cannot
    /// survive JSON; dxdb accepts a pattern string or `{$regex, $options}`.
    #[error("QU16: $regex for field {field} must be a pattern string or an object with $regex")]
    RegexObject {
        /// The field the regex applied to.
        field: String,
    },
    /// `QU17`: the query-builder cannot build `findByIds` queries.
    #[error("QU17: query-builder does not support findByIds")]
    FindByIds,
    /// A selector number a `f64` cannot hold exactly.
    #[error("{0} cannot be represented as a JS number without rounding")]
    UnrepresentableNumber(String),
    /// A `$meta` sort part, which [`SortPart`] cannot represent. See divergence 3.
    #[error("{{ {field}: {{$meta: {kind}}} }} is not expressible as a Mango sort part")]
    UnsortableMeta {
        /// The field being sorted.
        field: String,
        /// The `$meta` kind.
        kind: String,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The three attributes that chain but are dropped by the projection
/// (`nosql-query-builder.ts:459`).
const DROPPED_ATTRIBUTES: [&str; 3] = ["maxScan", "batchSize", "comment"];

/// [`SPECIAL_PROPERTIES`] from `mquery-utils.ts:10`.
const SPECIAL_PROPERTIES: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// A field path, as a `String` rather than `any`.
pub type Path = String;

/// The mutable query a builder owns.
///
/// The selector is held as ordered JSON because it is arbitrary JSON that upstream
/// mutates in place, and key order is observable to anything that later
/// serialises it. [`QueryBuilder::new`] requires the selector to be an object.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryBuilder {
    selector: Map<String, Value>,
    /// Current chaining path; `None` until `where_("field")`.
    path: Option<Path>,
    skip: Option<usize>,
    limit: Option<usize>,
    /// Sorts in the order they were pushed, each already reduced to a field and
    /// the direction value upstream stored.
    sort: Vec<(String, Value)>,
    /// Accepted-but-dropped attributes, kept so a caller can inspect them.
    dropped: Map<String, Value>,
}

/// The projection of a builder: `toJSON()` from `nosql-query-builder.ts:389`.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryBuilderJson {
    /// The `MangoQuery` under construction.
    pub query: MangoQuery,
    /// The current chaining path, so the next call can resume from it.
    pub path: Option<Path>,
}

impl Default for QueryBuilder {
    fn default() -> Self {
        Self::empty()
    }
}

impl QueryBuilder {
    /// `createQueryBuilder()` with no existing query (`nosql-query-builder.ts:629`).
    pub fn empty() -> Self {
        QueryBuilder {
            selector: Map::new(),
            path: None,
            skip: None,
            limit: None,
            sort: Vec::new(),
            dropped: Map::new(),
        }
    }

    /// `createQueryBuilder(query, path)` (`nosql-query-builder.ts:42-61`).
    ///
    /// Errors when the supplied selector is not an object. Upstream would coerce it
    /// into `_conditions` and fail somewhere less obvious later;
    /// `MangoQuery.selector` is documented as ordered JSON that normalisation has
    /// already rewritten, so a non-object here is a caller bug.
    pub fn new(query: &MangoQuery, path: Option<Path>) -> Result<Self> {
        let mut builder = QueryBuilder {
            path,
            ..QueryBuilder::empty()
        };
        builder.find(&query.selector)?;

        // `if (mangoQuery.limit)` and `if (mangoQuery.skip)` — an explicit `0` is
        // falsy upstream and so is *not* copied onto the builder. That is the
        // `limit-zero` family in testdata/vectors/normalised-queries.json, and it
        // is why the projection's `skip` is `0` rather than `None` when unset.
        if let Some(limit) = query.limit {
            if limit != 0 {
                builder.limit(limit);
            }
        }
        if query.skip != 0 {
            builder.skip(query.skip);
        }
        for part in &query.sort {
            builder.sort.push((
                part.field.clone(),
                Value::String(part.direction.as_str().into()),
            ));
        }
        Ok(builder)
    }

    // -- path and equality -------------------------------------------------

    /// `where(path)` (`nosql-query-builder.ts:67`).
    ///
    /// Sets the chaining path and returns `&mut Self` so calls chain. Rust has no
    /// overloads, so the two-argument shorthand is [`QueryBuilder::where_eq`] and
    /// the object form is [`QueryBuilder::where_object`].
    pub fn where_(&mut self, path: impl Into<Path>) -> &mut Self {
        self.path = Some(path.into());
        self
    }

    /// `where(path, value)` — shorthand equality (`:74-77`).
    pub fn where_eq(&mut self, path: impl Into<Path>, value: Value) -> &mut Self {
        let path = path.into();
        self.selector.insert(path.clone(), value);
        self.path = Some(path);
        self
    }

    /// `where(object)` — merge a conditions object (`:80-82`).
    pub fn where_object(&mut self, conditions: &Value) -> Result<&mut Self> {
        self.merge(conditions)
    }

    /// `equals(value)` (`:92`).
    pub fn equals(&mut self, value: Value) -> Result<&mut Self> {
        self.set_eq("equals", value)
    }

    /// `eq(value)` (`:106`). Separate from [`QueryBuilder::equals`] because both
    /// names are public upstream and a test asserts each.
    pub fn eq(&mut self, value: Value) -> Result<&mut Self> {
        self.set_eq("eq", value)
    }

    fn set_eq(&mut self, method: &'static str, value: Value) -> Result<&mut Self> {
        let path = self.require_path(method)?;
        self.operator_object(&path).insert("$eq".into(), value);
        Ok(self)
    }

    // -- logical operators -------------------------------------------------

    /// `or(...)` (`:121`): appends to `$or`, creating it when absent.
    pub fn or(&mut self, conditions: Vec<Value>) -> Result<&mut Self> {
        self.append_logical("$or", conditions)
    }

    /// `nor(...)` (`:133`).
    pub fn nor(&mut self, conditions: Vec<Value>) -> Result<&mut Self> {
        self.append_logical("$nor", conditions)
    }

    /// `and(...)` (`:146`).
    pub fn and(&mut self, conditions: Vec<Value>) -> Result<&mut Self> {
        self.append_logical("$and", conditions)
    }

    fn append_logical(&mut self, operator: &str, conditions: Vec<Value>) -> Result<&mut Self> {
        for condition in &conditions {
            if !is_plain_object(condition) {
                return Err(Error::MergeNotAnObject {
                    value: condition.to_string(),
                });
            }
        }
        let entry = self
            .selector
            .entry(operator.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        match entry {
            Value::Array(array) => array.extend(conditions),
            _ => {
                return Err(Error::MergeNotAnObject {
                    value: format!("{operator} already holds a non-array value"),
                })
            }
        }
        Ok(self)
    }

    // -- comparisons -------------------------------------------------------

    /// `gt(value)` — one of the generated `OTHER_MANGO_OPERATORS`
    /// (`nosql-query-builder.ts:473`), which take the path from the last `where()`.
    pub fn gt(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("gt", value)
    }
    /// `gte`.
    pub fn gte(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("gte", value)
    }
    /// `lt`.
    pub fn lt(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("lt", value)
    }
    /// `lte`.
    pub fn lte(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("lte", value)
    }
    /// `ne`.
    pub fn ne(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("ne", value)
    }
    /// `in`.
    pub fn in_(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("in", value)
    }
    /// `nin`.
    pub fn nin(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("nin", value)
    }
    /// `size`.
    pub fn size(&mut self, value: Value) -> Result<&mut Self> {
        self.comparison("size", value)
    }

    /// `regex(pattern)` (`:510-529`).
    ///
    /// A string becomes `$regex`; the object form carries `$regex` and may carry
    /// `$options`. A regex *object* is `QU16` upstream because it cannot survive
    /// JSON, and is simply not a representable argument here.
    pub fn regex(&mut self, pattern: RegexArg) -> Result<&mut Self> {
        let path = self.require_path("regex")?;
        let conds = self.operator_object(&path);
        match pattern {
            RegexArg::Pattern(pattern) => {
                conds.insert("$regex".into(), Value::String(pattern));
            }
            RegexArg::PatternWithOptions { pattern, options } => {
                conds.insert("$regex".into(), Value::String(pattern));
                conds.insert("$options".into(), Value::String(options));
            }
        }
        Ok(self)
    }

    fn comparison(&mut self, operator: &'static str, value: Value) -> Result<&mut Self> {
        let path = self.require_path(operator)?;
        self.operator_object(&path)
            .insert(format!("${operator}"), value);
        Ok(self)
    }

    /// `mod(path, [divisor, remainder])` (`:156`).
    pub fn mod_(
        &mut self,
        path: impl Into<Path>,
        divisor: Value,
        remainder: Value,
    ) -> Result<&mut Self> {
        self.operator_object(&path.into())
            .insert("$mod".into(), Value::Array(vec![divisor, remainder]));
        Ok(self)
    }

    /// `exists(path, value)` (`:189`).
    pub fn exists(&mut self, path: impl Into<Path>, value: bool) -> Result<&mut Self> {
        self.operator_object(&path.into())
            .insert("$exists".into(), Value::Bool(value));
        Ok(self)
    }

    /// `exists(path)` / `exists()` — always `true` (`:193-203`).
    pub fn exists_field(&mut self, path: Option<Path>) -> Result<&mut Self> {
        let path = match path {
            Some(path) => path,
            None => self.require_path("exists")?,
        };
        self.operator_object(&path)
            .insert("$exists".into(), Value::Bool(true));
        Ok(self)
    }

    /// `elemMatch(path, criteria)` (`:229`).
    pub fn elem_match(&mut self, path: impl Into<Path>, criteria: Value) -> Result<&mut Self> {
        if !is_plain_object(&criteria) {
            return Err(Error::ElemMatchArguments);
        }
        self.operator_object(&path.into())
            .insert("$elemMatch".into(), criteria);
        Ok(self)
    }

    /// `elemMatch(path, callback)` — the callback form (`:255-259`).
    pub fn elem_match_with<F>(&mut self, path: impl Into<Path>, build: F) -> Result<&mut Self>
    where
        F: FnOnce(&mut QueryBuilder) -> Result<()>,
    {
        let mut inner = QueryBuilder::empty();
        build(&mut inner)?;
        self.operator_object(&path.into())
            .insert("$elemMatch".into(), Value::Object(inner.selector));
        Ok(self)
    }

    /// The shared body of `equals`/`eq` and the generated operators: keep an
    /// existing condition object, or promote a shorthand value to `{$eq: value}` so
    /// both coexist (`nosql-query-builder.ts:492-505`).
    fn operator_object(&mut self, path: &str) -> &mut Map<String, Value> {
        let entry = self
            .selector
            .entry(path.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        // A shorthand value — including `null` — becomes `{$eq: value}`, which is
        // why the test for a shorthand is "not an object" and not "not null".
        if !matches!(entry, Value::Object(_)) {
            let shorthand = entry.clone();
            let mut conds = Map::new();
            conds.insert("$eq".into(), shorthand);
            *entry = Value::Object(conds);
        }
        match entry {
            Value::Object(map) => map,
            // `entry` was replaced with an object above.
            _ => unreachable!("operator_object left a non-object in the selector"),
        }
    }

    fn require_path(&self, method: &'static str) -> Result<Path> {
        self.path.clone().ok_or(Error::NoPath { method })
    }

    // -- options -----------------------------------------------------------

    /// `limit(n)` (`:459`). The projection emits it whenever it is a number,
    /// including an explicit `0` (`:400`).
    pub fn limit(&mut self, limit: usize) -> &mut Self {
        self.limit = Some(limit);
        self
    }

    /// `skip(n)` (`:459`). Same `0` rule as [`QueryBuilder::limit`].
    pub fn skip(&mut self, skip: usize) -> &mut Self {
        self.skip = Some(skip);
        self
    }

    /// `maxScan`, `batchSize`, `comment` (`:459`).
    ///
    /// Accepted and discarded by the projection, exactly as upstream. See crate doc
    /// divergence 1.
    pub fn dropped_attribute(&mut self, name: &str, value: Value) -> Result<&mut Self> {
        if !DROPPED_ATTRIBUTES.contains(&name) {
            return Err(Error::SortArguments {
                args: format!("unknown attribute {name}"),
            });
        }
        self.dropped.insert(name.to_string(), value);
        Ok(self)
    }

    /// The dropped attributes, for a caller that wants to see what was accepted.
    pub fn dropped_attributes(&self) -> &Map<String, Value> {
        &self.dropped
    }

    // -- sort --------------------------------------------------------------

    /// `sort(object)` / `sort(array)` / `sort(string)` (`:275-311`).
    ///
    /// All three upstream shapes are accepted and kept in insertion order.
    pub fn sort(&mut self, arg: SortArg) -> Result<&mut Self> {
        match arg {
            SortArg::Object(entries) => {
                for (field, value) in entries {
                    self.push_sort(&field, value)?;
                }
            }
            SortArg::Array(entries) => {
                for (field, value) in entries {
                    self.sort.push((field, value));
                }
            }
            SortArg::String(spec) => {
                // `.split(/\s+/)`: tokens, with a `-` prefix meaning descending
                // (`:294-300`).
                for token in spec.split_whitespace() {
                    match token.strip_prefix('-') {
                        Some(field) => self.push_sort(field, Value::from(-1))?,
                        None => self.push_sort(token, Value::from(1))?,
                    }
                }
            }
        }
        Ok(self)
    }

    /// `push` (`:533`): validate one direction, store `$meta` opaquely.
    fn push_sort(&mut self, field: &str, value: Value) -> Result<()> {
        if let Value::Object(map) = &value {
            if let Some(meta) = map.get("$meta") {
                self.sort.push((
                    field.to_string(),
                    Value::Object(Map::from_iter([("$meta".to_string(), meta.clone())])),
                ));
                return Ok(());
            }
        }
        let direction = parse_sort_direction(&value).ok_or_else(|| Error::SortDirection {
            field: field.to_string(),
            // `if (Array.isArray(value)) value = '[' + value + ']'` (`:563`).
            value: match &value {
                Value::Array(items) => format!(
                    "[{}]",
                    items
                        .iter()
                        .map(scalar_to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                other => scalar_to_string(other),
            },
        })?;
        self.sort
            .push((field.to_string(), Value::String(direction.as_str().into())));
        Ok(())
    }

    // -- merge -------------------------------------------------------------

    /// `merge(source)` (`:322`).
    pub fn merge(&mut self, source: &Value) -> Result<&mut Self> {
        if !is_plain_object(source) {
            return Err(Error::MergeNotAnObject {
                value: source.to_string(),
            });
        }
        let normalized = normalize_shorthands_before_merge(&mut self.selector, source);
        merge_conditions(&mut self.selector, &normalized);
        Ok(self)
    }

    /// `find(criteria)` (`:368`): merge when the criteria can be merged, ignore
    /// otherwise (`canMerge`, `:624`).
    pub fn find(&mut self, criteria: &Value) -> Result<&mut Self> {
        if is_plain_object(criteria) {
            self.merge(criteria)?;
        }
        Ok(self)
    }

    // -- projection --------------------------------------------------------

    /// `toJSON()` (`nosql-query-builder.ts:389`).
    ///
    /// Emits the selector, plus `skip`, `limit` and `sort` only when they were set;
    /// `sort` is normalised to `[{field: 'asc'|'desc'}]` here and nowhere else.
    pub fn to_json(&self) -> Result<QueryBuilderJson> {
        let mut sort = Vec::with_capacity(self.sort.len());
        for (field, value) in &self.sort {
            if let Value::Object(map) = value {
                if let Some(meta) = map.get("$meta") {
                    return Err(Error::UnsortableMeta {
                        field: field.clone(),
                        kind: scalar_to_string(meta),
                    });
                }
            }
            let direction = parse_sort_direction(value).ok_or_else(|| Error::SortDirection {
                field: field.clone(),
                value: scalar_to_string(value),
            })?;
            sort.push(SortPart {
                field: field.clone(),
                direction,
            });
        }
        Ok(QueryBuilderJson {
            query: MangoQuery {
                selector: Value::Object(self.selector.clone()),
                sort,
                skip: self.skip.unwrap_or(0),
                limit: self.limit,
                index: None,
            },
            path: self.path.clone(),
        })
    }

    /// The accumulated selector, for a caller that does not need the full query.
    pub fn selector(&self) -> Value {
        Value::Object(self.selector.clone())
    }

    /// The current chaining path.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// `findByIds` guard (`index.ts:42`): the builder cannot express it.
    pub fn find_by_ids(&self) -> Result<QueryBuilderJson> {
        Err(Error::FindByIds)
    }
}

/// A `$regex` argument (`nosql-query-builder.ts:510`).
#[derive(Clone, Debug, PartialEq)]
pub enum RegexArg {
    /// A bare pattern string.
    Pattern(String),
    /// `{$regex: pattern, $options: flags}`.
    PatternWithOptions {
        /// The pattern.
        pattern: String,
        /// ECMAScript flags, e.g. `"i"`.
        options: String,
    },
}

/// The three `sort()` argument shapes upstream accepts (`:275-311`).
#[derive(Clone, Debug, PartialEq)]
pub enum SortArg {
    /// `{field: 1, other: -1}`, order preserved.
    Object(Vec<(String, Value)>),
    /// `[['field', 1], ['other', -1]]`.
    Array(Vec<(String, Value)>),
    /// `"field -other"`.
    String(String),
}

/// Upstream's direction dialect (`:557`): `String(value || 1).toLowerCase()` must
/// match `^(?:ascending|asc|descending|desc|1|-1)$`.
///
/// Note `value || 1`, so `0`, `""`, `null` and `false` all mean ascending.
fn parse_sort_direction(value: &Value) -> Option<SortDirection> {
    let falsy = match value {
        Value::Null | Value::Bool(false) => true,
        Value::Number(number) => number.as_f64() == Some(0.0),
        _ => false,
    };
    let raw = if falsy {
        "1".to_string()
    } else {
        scalar_to_string(value)
    };
    match raw.to_lowercase().as_str() {
        "ascending" | "asc" | "1" => Some(SortDirection::Asc),
        "descending" | "desc" | "-1" => Some(SortDirection::Desc),
        _ => None,
    }
}

/// `isObject` from `mquery-utils.ts:35`: a plain object, not an array, `null` or a
/// primitive.
fn is_plain_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// `normalizeShorthandsBeforeMerge` (`nosql-query-builder.ts:599`).
///
/// A selector field holds either a shorthand (`{age: 7}`) or an operator object
/// (`{age: {$gt: 3}}`). When the two sides of a merge disagree, the shorthand side
/// is promoted to `{$eq: value}` so the merge can keep both. The `conditions` side
/// is mutated in place; the returned value is a flat copy of `source` with the
/// incoming side already promoted, which is what upstream hands to `merge`.
fn normalize_shorthands_before_merge(conditions: &mut Map<String, Value>, source: &Value) -> Value {
    let Value::Object(source) = source else {
        return source.clone();
    };
    let mut normalized = source.clone();
    for (field, incoming) in source {
        // `if (field.startsWith('$')) return;` — logical operators are not fields.
        if field.starts_with('$') {
            continue;
        }
        let Some(existing) = conditions.get(field).cloned() else {
            continue;
        };
        // `isOperatorObject` is `typeof v === 'object' && v !== null &&
        // !Array.isArray(v)`, i.e. any plain object, not only `$`-prefixed ones.
        let existing_is_operator = existing.is_object();
        let incoming_is_operator = incoming.is_object();
        if existing_is_operator && !incoming_is_operator {
            normalized.insert(
                field.clone(),
                Value::Object(Map::from_iter([("$eq".to_string(), incoming.clone())])),
            );
        } else if !existing_is_operator && incoming_is_operator {
            conditions.insert(
                field.clone(),
                Value::Object(Map::from_iter([("$eq".to_string(), existing)])),
            );
        }
    }
    Value::Object(normalized)
}

/// `merge` from `mquery-utils.ts:15`.
///
/// Recurses when both the existing and incoming values are objects; otherwise the
/// incoming value replaces the existing one (`mquery-utils.ts:21-28`). Skips the
/// prototype-pollution keys.
fn merge_conditions(to: &mut Map<String, Value>, from: &Value) {
    let Value::Object(from) = from else {
        return;
    };
    for (key, value) in from {
        if SPECIAL_PROPERTIES.contains(&key.as_str()) {
            continue;
        }
        match to.get_mut(key) {
            // `typeof to[key] === 'undefined'`: new key, assign.
            None => {
                to.insert(key.clone(), value.clone());
            }
            Some(Value::Object(existing)) => {
                // `if (isObject(from[key])) merge(to[key], from[key])`.
                if value.is_object() {
                    merge_conditions(existing, value);
                } else {
                    *to.get_mut(key).expect("checked above") = value.clone();
                }
            }
            // `:24-27` calls `merge` on a primitive, which JS silently no-ops on,
            // so the effective behaviour is replacement.
            Some(_) => {
                *to.get_mut(key).expect("checked above") = value.clone();
            }
        }
    }
}

/// The string form upstream's error messages embed.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Validates that a selector number is one JS can hold, and returns it unchanged.
///
/// This does **not** rewrite the value. A JS number has no integer type, so
/// `5` and `5.0` are the same number; rewriting a JSON integer into a float would
/// change the serialised form of a selector for no semantic gain, and the vector
/// oracle would notice. What the check buys is the one thing `f64` cannot do:
/// an integer outside the safe range would be silently rounded by JS, so it is
/// [`Error::UnrepresentableNumber`] here instead of a query that quietly matches
/// the wrong documents.
pub fn js_number(value: Value) -> Result<Value> {
    if let Value::Number(number) = &value {
        // An `f64`-typed JSON number is already exact.
        if number.is_f64() {
            return Ok(value);
        }
        let as_float = number
            .as_f64()
            .ok_or_else(|| Error::UnrepresentableNumber(number.to_string()))?;
        if as_float.abs() > 9_007_199_254_740_991.0 {
            return Err(Error::UnrepresentableNumber(number.to_string()));
        }
    }
    Ok(value)
}

/// Re-exported so a caller can render a selector the way `JSON.stringify` would,
/// which is how the vector oracle compares queries.
pub use dxdb_json::to_js_json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_carries_skip_zero_and_no_limit_by_default() {
        let json = QueryBuilder::empty().to_json().expect("to_json");
        assert_eq!(json.query.skip, 0);
        assert_eq!(json.query.limit, None);
        assert_eq!(json.query.selector, Value::Object(Map::new()));
        assert!(json.query.sort.is_empty());
        assert_eq!(json.path, None);
    }

    #[test]
    fn parse_sort_direction_follows_the_upstream_regex() {
        assert_eq!(
            parse_sort_direction(&Value::from("asc")),
            Some(SortDirection::Asc)
        );
        assert_eq!(
            parse_sort_direction(&Value::from("ASCENDING")),
            Some(SortDirection::Asc)
        );
        assert_eq!(
            parse_sort_direction(&Value::from(1)),
            Some(SortDirection::Asc)
        );
        assert_eq!(
            parse_sort_direction(&Value::from(-1)),
            Some(SortDirection::Desc)
        );
        assert_eq!(
            parse_sort_direction(&Value::from("desc")),
            Some(SortDirection::Desc)
        );
        assert_eq!(parse_sort_direction(&Value::from("bogus")), None);
        assert_eq!(parse_sort_direction(&Value::from(2)), None);
    }
}

// ---------------------------------------------------------------------------
// The plugin, on the core registry
// ---------------------------------------------------------------------------

/// The hooks this plugin registers.
///
/// Empty, and that is upstream's shape: `RxDBQueryBuilderPlugin`
/// (`src/plugins/query-builder/index.ts:54-81`) has a `prototypes` block and **no**
/// `hooks` block. The builder is a pure computation over a `MangoQuery`, so there is
/// nothing for the registry to dispatch.
///
/// It is worth being explicit that this plugin reads a *hook-adjacent* thing without
/// registering one: upstream's `applyBuildingStep` consults `overwritable.isDevMode()`
/// to reject `findByIds` chaining (`index.ts:42-47`). That is the dev-mode overwritable,
/// available from [`dxdb_core::Overwritables::is_dev_mode`], not a hook.
pub const HOOKS: &[dxdb_core::HookName] = &[];

/// The `RxDBQueryBuilderPlugin` equivalent.
///
/// Install with `database.add_plugin(QueryBuilderPlugin)?`. Behaviour arrives through
/// [`QueryBuilder`], the extension-trait spelling of upstream's prototype methods.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueryBuilderPlugin;

impl dxdb_core::Plugin for QueryBuilderPlugin {
    fn name(&self) -> &'static str {
        // The upstream plugin's `name` field (`src/plugins/query-builder/index.ts:55`).
        "query-builder"
    }

    fn install(
        &self,
        _registrations: &mut dxdb_core::Registrations,
        _overwritables: &mut dxdb_core::Overwritables,
    ) -> std::result::Result<(), dxdb_core::PluginError> {
        Ok(())
    }
}

/// Chains a building step onto an existing [`MangoQuery`], the way
/// `runBuildingStep` does (`src/plugins/query-builder/index.ts:15-35`).
///
/// Upstream clones the query, builds a `NoSqlQueryBuilder` from it, applies one method,
/// and returns a *new* `RxQuery` carrying the accumulated `queryBuilderPath` in its
/// `other` bag. The Rust equivalent returns a new [`QueryBuilder`] seeded from the query
/// and its previous path, which is the same value without the `RxQuery` wrapper (that
/// needs lane D's query handle).
pub fn continue_building(
    query: &MangoQuery,
    previous_path: Option<&str>,
    step: impl FnOnce(&mut QueryBuilder) -> Result<()>,
) -> Result<QueryBuilderJson> {
    let mut builder = QueryBuilder::new(query, previous_path.map(str::to_string))?;
    step(&mut builder)?;
    builder.to_json()
}
