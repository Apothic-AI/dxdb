//! The Mango selector matcher.
//!
//! This is a port of *mingo as RxDB configures it*, not mingo in general. The
//! configuration is `src/rx-query-mingo.ts:36`: a `Context` with exactly the
//! operators RxDB lists, and `new Query(selector, { context })`. Each assertion
//! below cites the implementation it mirrors — `mingo@7.2.5`,
//! `esm/operators/_predicates.js`, `esm/util/_internal.js` and `esm/query.js` —
//! because "match mingo" without a version is not a specification.
//!
//! # What matching means
//!
//! A selector is a JSON object. Every top-level key must hold, so the outer
//! combinator is `AND`:
//!
//! - a key starting with `$` is a top-level operator: only `$and`, `$or` and
//!   `$nor` are accepted; anything else throws
//!   (`esm/query.js`: `unknown top level operator`).
//! - any other key is a field path. Its value is normalised: a non-object, or an
//!   object with no operator keys, becomes `{$eq: value}`; an object with
//!   operator keys is used as-is (`util.normalize`).
//!
//! # Field resolution has three behaviours worth spelling out
//!
//! `resolve(o, path, {unwrapArray: true})` (`esm/util/_internal.js`):
//!
//! 1. A path with no dot returns the property directly, **even if it is an
//!    array** — the array is not descended for path resolution.
//! 2. A path into an array of objects, where the next segment is not a numeric
//!    index, maps over the array and collects non-`undefined` results. That is
//!    why `{'a.b': 1}` matches `{a: [{b: 1}]}`.
//! 3. `unwrapArray` unwraps a single-element array of arrays, which is why a
//!    doubly-nested match needs depth, not just flattening.
//!
//! # Equality is not `==`
//!
//! mingo's `isEqual` compares structurally, ignores key order, and requires the
//! same constructor. `$eq` adds two things on top (`$eq` in `_predicates.js`):
//! `null` and `undefined` are equal to each other, and a document-side array
//! matches if **any** element equals the operand, or if the operand matches after
//! [`flatten`]ing to the query's path depth. So `{tags: {$eq: 'a'}}` matches
//! `{tags: ['a', 'b']}`. That is upstream behaviour, not a bug to fix.
//!
//! # Ordering comparisons use mingo's cross-type order
//!
//! `$gt`/`$gte`/`$lt`/`$lte` only compare when both sides have the same mingo
//! *type* (`$lt` in `_predicates.js`: `typeOf(v) === typeOf(b)`). When the types
//! differ the predicate is false, not "one sorts before the other". Within a type,
//! comparison is mingo's `compare` (`mingoCmp`), which is **not** JavaScript's
//! `<`: numbers compare numerically, strings by code unit, arrays element-wise
//! after sorting, objects key-by-key with sorted keys.
//!
//! # `$regex` is ECMAScript
//!
//! `$regex` compiles with [`regress`], which targets the ECMAScript grammar. Flags
//! come from `$options` and become a [`RegexFlags`] value, never inline `(?i)`:
//! `(?i)` is not ECMAScript syntax, and a selector containing it must fail the
//! same way it fails in a browser.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde_json::{Map, Value};

use crate::error::{QueryError, QueryErrorCode};

/// The default number of path separators mingo uses for array flattening when a
/// path has no dots (`processQuery`: `depth` counts the separators, and
/// `$eq` reads `options?.local?.depth ?? 1`).
pub const MINGO_DEFAULT_DEPTH: usize = 1;

/// A compiled `$regex`.
///
/// `regress::Regex` is neither `Clone` nor cheap to build, so compiled patterns
/// are shared through an [`Rc`] and cached per thread.
#[derive(Debug)]
pub struct RegexMatcher {
    pattern: String,
    flags: RegexFlags,
    inner: regress::Regex,
}

impl RegexMatcher {
    /// Compiles a pattern with the given flags.
    pub fn new(pattern: &str, flags: RegexFlags) -> Result<Self, QueryError> {
        let inner = regress::Regex::with_flags(
            pattern,
            regress::Flags {
                icase: flags.icase,
                multiline: flags.multiline,
                dot_all: flags.dot_all,
                unicode: flags.unicode,
                unicode_sets: flags.unicode_sets,
                no_opt: false,
            },
        )
        .map_err(|error| {
            QueryError::new(
                QueryErrorCode::InvalidRegex,
                format!(
                    "$regex {pattern:?} with flags {:?}: {error}",
                    flags.as_str()
                ),
            )
        })?;
        Ok(Self {
            pattern: pattern.to_string(),
            flags,
            inner,
        })
    }

    /// The pattern as given.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The flags in effect.
    pub fn flags(&self) -> RegexFlags {
        self.flags
    }

    /// True when the pattern matches anywhere in `text`, which is what
    /// `RegExp.prototype.test` does without the `g` flag.
    pub fn is_match(&self, text: &str) -> bool {
        self.inner.find(text).is_some()
    }
}

/// JavaScript regex flags, as a struct rather than inline pattern syntax.
///
/// PLAN.md section 8 verified this shape against `regress` 0.12: `(?i)` is not
/// part of ECMAScript and must not be accepted, while `\p{Letter}` needs
/// `unicode: true`.
///
/// `g` and `y` parse but do not change matching. A JavaScript regex with `g` is
/// stateful through `lastIndex`; mingo's `$regex` calls `exec` once per candidate
/// and never inspects `lastIndex`, so the only observable difference would be a
/// bug. `y` (sticky) would need an anchoring behaviour that mingo does not ask
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct RegexFlags {
    /// `i`
    pub icase: bool,
    /// `m`
    pub multiline: bool,
    /// `s`
    pub dot_all: bool,
    /// `u`
    pub unicode: bool,
    /// `v`
    pub unicode_sets: bool,
    /// `g`, parsed and ignored.
    pub global: bool,
    /// `y`, parsed and ignored.
    pub sticky: bool,
}

impl RegexFlags {
    /// No flags.
    pub fn none() -> Self {
        Self::default()
    }

    /// Parses a `$options` string the way `new RegExp(pattern, options)` does.
    ///
    /// An unrecognised flag is an error rather than being ignored:
    /// `new RegExp('a', 'q')` throws `SyntaxError` in JavaScript, and a query that
    /// throws there must throw here or the two stacks disagree about which
    /// queries are legal.
    pub fn parse(options: &str) -> Result<Self, QueryError> {
        let mut flags = Self::default();
        for ch in options.chars() {
            match ch {
                'i' => flags.icase = true,
                'm' => flags.multiline = true,
                's' => flags.dot_all = true,
                'u' => flags.unicode = true,
                'v' => flags.unicode_sets = true,
                'g' => flags.global = true,
                'y' => flags.sticky = true,
                other => {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidRegex,
                        format!("unsupported regex flag {other:?} in $options {options:?}"),
                    ))
                }
            }
        }
        Ok(flags)
    }

    /// The flags in the order `RegExp.prototype.flags` returns them (`dgimsuvy`).
    pub fn as_str(&self) -> String {
        let mut out = String::new();
        if self.global {
            out.push('g');
        }
        if self.icase {
            out.push('i');
        }
        if self.multiline {
            out.push('m');
        }
        if self.dot_all {
            out.push('s');
        }
        if self.unicode {
            out.push('u');
        }
        if self.unicode_sets {
            out.push('v');
        }
        if self.sticky {
            out.push('y');
        }
        out
    }
}

thread_local! {
    /// Per-thread compiled-pattern cache.
    ///
    /// A query is matched against every candidate document, so the same pattern
    /// would otherwise be parsed per document. Thread-local rather than global
    /// because a `regress::Regex` is not worth sharing across threads and a
    /// `RefCell` avoids putting interior mutability in the public type.
    static REGEX_CACHE: RefCell<HashMap<(String, RegexFlags), Rc<RegexMatcher>>> =
        RefCell::new(HashMap::new());
}

/// Compiles through the thread-local cache.
fn compile_regex(pattern: &str, flags: RegexFlags) -> Result<Rc<RegexMatcher>, QueryError> {
    let key = (pattern.to_string(), flags);
    if let Some(found) = REGEX_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Ok(found);
    }
    let compiled = Rc::new(RegexMatcher::new(pattern, flags)?);
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        // A long-lived process should not accumulate every pattern it has ever
        // seen; the cap is generous enough that a query corpus stays warm.
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(key, compiled.clone());
    });
    Ok(compiled)
}

/// A compiled selector, reusable across documents.
///
/// Compilation is where the operator set is validated and where regex patterns are
/// parsed, so building a `Matcher` once per query and calling [`Matcher::matches`]
/// per document is both the fast path and the path that rejects a bad selector
/// before any row is touched.
#[derive(Debug)]
pub struct Matcher {
    root: CompiledSelector,
}

impl Matcher {
    /// Compiles a selector, validating the operator set.
    ///
    /// An unknown operator anywhere in the selector is
    /// [`QueryErrorCode::UnknownOperator`], matching mingo's throw.
    pub fn new(selector: &Value) -> Result<Self, QueryError> {
        Ok(Self {
            root: compile_selector(selector, 0)?,
        })
    }

    /// Compiles a field-level operator object, for callers that already know the
    /// shape (`$elemMatch` arms and direct value tests).
    pub fn compile_field_value(value: &Value) -> Result<Matcher, QueryError> {
        Ok(Self {
            root: CompiledSelector {
                fields: Vec::new(),
                and: Vec::new(),
                or: Vec::new(),
                nor: Vec::new(),
                any_or: false,
                field: Some(compile_field_value_inner(value, 0)?),
            },
        })
    }

    /// Tests one document, answering only yes or no.
    pub fn matches(&self, document: &Value) -> bool {
        self.root.test(document)
    }

    /// Tests a bare value against a compiled field-level matcher, for the
    /// [`Matcher::compile_field_value`] shape.
    pub fn matches_value(&self, value: Option<&Value>) -> bool {
        match &self.root.field {
            Some(field) => field.test(value, MINGO_DEFAULT_DEPTH),
            None => false,
        }
    }

}

/// A selector after normalisation, with `$regex` patterns compiled.
#[derive(Debug, Clone)]
pub(crate) struct CompiledSelector {
    /// Field predicates, evaluated as a conjunction.
    fields: Vec<CompiledField>,
    /// `$and` arms.
    and: Vec<CompiledSelector>,
    /// `$or` arms.
    or: Vec<CompiledSelector>,
    /// `$nor` arms.
    nor: Vec<CompiledSelector>,
    /// `$or: []` was present. `some` on an empty array is false, so an explicit
    /// empty `$or` matches nothing while an absent one matches everything.
    any_or: bool,
    /// Set only for the [`Matcher::compile_field_value`] shape.
    field: Option<CompiledFieldValue>,
}

/// One field path and its operators.
#[derive(Debug, Clone)]
pub(crate) struct CompiledField {
    path: String,
    value: CompiledFieldValue,
}

/// The operator set for one field.
///
/// Explicit named fields rather than a map: the hot path avoids hashing and the
/// supported set (`src/types/rx-query.d.ts:43`) stays visible in one place.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompiledFieldValue {
    eq: Option<Value>,
    ne: Option<Value>,
    gt: Option<Value>,
    gte: Option<Value>,
    lt: Option<Value>,
    lte: Option<Value>,
    in_values: Option<Vec<Value>>,
    nin_values: Option<Vec<Value>>,
    exists: Option<bool>,
    type_names: Option<Vec<String>>,
    modulus: Option<(f64, f64)>,
    not: Option<Box<CompiledFieldValue>>,
    size: Option<usize>,
    elem_match: Option<Box<CompiledElemMatch>>,
    regex: Option<Rc<RegexMatcher>>,
}

/// The `$elemMatch` payload, in either of the two shapes mingo builds.
#[derive(Debug, Clone)]
enum CompiledElemMatch {
    /// Every key was a query operator other than `$and`/`$or`/`$nor`, so each
    /// element is substituted for the field (`elemMatchPredicate`'s `wrap`).
    AsValue(CompiledFieldValue),
    /// Keys included a field name or a logical operator, so each element is
    /// matched as an object against this selector.
    AsSelector(CompiledSelector),
    /// The criteria was `{}`, which mingo treats as "the element is an object".
    OnlyObjects,
}

/// The recursion limit for nested selectors. A legal selector nests a handful of
/// levels; an unbounded chain is a stack overflow, so the limit is explicit and
/// reported as an error rather than a crash.
const MAX_SELECTOR_DEPTH: usize = 64;

fn compile_selector(selector: &Value, depth: usize) -> Result<CompiledSelector, QueryError> {
    if depth > MAX_SELECTOR_DEPTH {
        return Err(QueryError::new(
            QueryErrorCode::InvalidOperatorPayload,
            "selector nesting is too deep",
        ));
    }
    let Some(object) = selector.as_object() else {
        return Err(QueryError::new(
            QueryErrorCode::InvalidOperatorPayload,
            format!("selector must be an object, got {selector}"),
        ));
    };

    let mut compiled = CompiledSelector {
        fields: Vec::new(),
        and: Vec::new(),
        or: Vec::new(),
        nor: Vec::new(),
        any_or: false,
        field: None,
    };

    for (key, value) in object {
        match key.as_str() {
            "$and" | "$or" | "$nor" => {
                let Some(arms) = value.as_array() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        format!("{key} expects an array"),
                    ));
                };
                let mut compiled_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    compiled_arms.push(compile_selector(arm, depth + 1)?);
                }
                match key.as_str() {
                    "$and" => compiled.and = compiled_arms,
                    "$or" => {
                        compiled.or = compiled_arms;
                        compiled.any_or = true;
                    }
                    _ => compiled.nor = compiled_arms,
                }
            }
            other if other.starts_with('$') => {
                return Err(QueryError::new(
                    QueryErrorCode::UnknownTopLevelOperator,
                    format!("unknown top level operator: {other}"),
                ));
            }
            field => compiled.fields.push(CompiledField {
                path: field.to_string(),
                value: compile_field_value_inner(value, depth + 1)?,
            }),
        }
    }

    Ok(compiled)
}

/// mingo's `isOperator`: `$` followed by at least one alphanumeric or `_`
/// (`/^\$[a-zA-Z0-9_]+$/`).
fn is_operator(key: &str) -> bool {
    let mut chars = key.chars();
    if chars.next() != Some('$') {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    !rest.is_empty()
        && rest
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '_')
}

/// `util.normalize`: a scalar becomes `{$eq: scalar}`, and an object with no
/// operator key becomes `{$eq: object}`.
///
/// One thing is deliberately *not* reproduced: mingo's `normalize` also converts a
/// `RegExp` instance to `{$regex: <RegExp>}`, and a `$regex` object into a compiled
/// `RegExp`. JSON has no regex literal, upstream rejects a `RegExp` in a query in
/// dev mode (`QU16`, `check-query.ts:213`), and a `$regex` without `$options`
/// compiles identically whether mingo wraps it or not. So the only form that
/// reaches this crate is the string pattern, and it is compiled here.
fn normalize_field_value(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.keys().any(|key| is_operator(key)) => value.clone(),
        other => {
            let mut map = Map::new();
            map.insert("$eq".to_string(), other.clone());
            Value::Object(map)
        }
    }
}

fn compile_field_value_inner(
    value: &Value,
    depth: usize,
) -> Result<CompiledFieldValue, QueryError> {
    let normalized = normalize_field_value(value);
    let Some(object) = normalized.as_object() else {
        return Err(QueryError::new(
            QueryErrorCode::InvalidOperatorPayload,
            "field value normalised to a non-object",
        ));
    };

    let mut out = CompiledFieldValue::default();
    let mut options: Option<String> = None;
    // `$regex` may appear before or after `$options` in key order, and JSON object
    // order is not something a caller should have to think about, so the pattern
    // is compiled after the loop once both are known.
    let mut regex_pattern: Option<String> = None;

    for (operator, operand) in object {
        match operator.as_str() {
            "$eq" => out.eq = Some(operand.clone()),
            "$ne" => out.ne = Some(operand.clone()),
            "$gt" => out.gt = Some(operand.clone()),
            "$gte" => out.gte = Some(operand.clone()),
            "$lt" => out.lt = Some(operand.clone()),
            "$lte" => out.lte = Some(operand.clone()),
            "$in" => {
                let Some(values) = operand.as_array() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        "$in expects an array",
                    ));
                };
                out.in_values = Some(values.clone());
            }
            "$nin" => {
                let Some(values) = operand.as_array() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        "$nin expects an array",
                    ));
                };
                out.nin_values = Some(values.clone());
            }
            // mingo's `$exists` reads `!!value`, not `value === true`
            // (`element/exists.js`: `const b = !!value`). That matters for a
            // selector written as `{$exists: 0}`, which is false.
            "$exists" => out.exists = Some(operand.as_bool().unwrap_or_else(|| truthy(operand))),
            "$type" => {
                out.type_names = Some(match operand {
                    Value::Array(items) => {
                        let mut names = Vec::with_capacity(items.len());
                        for item in items {
                            names.push(match item {
                                Value::String(s) => s.clone(),
                                Value::Number(n) => {
                                    dxdb_jsnum::to_js_string(n.as_f64().unwrap_or_default())
                                }
                                other => {
                                    return Err(QueryError::new(
                                        QueryErrorCode::InvalidOperatorPayload,
                                        format!("$type entry {other} is not a string or number"),
                                    ))
                                }
                            });
                        }
                        names
                    }
                    Value::String(s) => vec![s.clone()],
                    Value::Number(n) => {
                        vec![dxdb_jsnum::to_js_string(n.as_f64().unwrap_or_default())]
                    }
                    other => {
                        return Err(QueryError::new(
                            QueryErrorCode::InvalidOperatorPayload,
                            format!("$type {other} is not a string, number or array"),
                        ))
                    }
                });
            }
            "$mod" => {
                // `$mod` in `_predicates.js`: `b.length === 2 && x % b[0] === b[1]`.
                // A payload of any other length simply never matches; it is not an
                // error. Reported as an error here because a malformed `$mod` is a
                // programming mistake and silently matching nothing hides it — the
                // one place this crate is stricter than upstream, noted in the
                // test that pins it.
                let Some(items) = operand.as_array() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        "$mod expects an array of two numbers",
                    ));
                };
                if items.len() != 2 {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        "$mod expects exactly two numbers",
                    ));
                }
                let divisor = items[0].as_f64().ok_or_else(|| {
                    QueryError::new(QueryErrorCode::InvalidOperatorPayload, "$mod divisor")
                })?;
                let remainder = items[1].as_f64().ok_or_else(|| {
                    QueryError::new(QueryErrorCode::InvalidOperatorPayload, "$mod remainder")
                })?;
                out.modulus = Some((divisor, remainder));
            }
            // `logical/not.js`: `criteria[selector] = normalize(rhs)` then a
            // sub-query, so `{$not: <scalar>}` and `{$not: {...}}` are the same
            // code path.
            "$not" => out.not = Some(Box::new(compile_field_value_inner(operand, depth + 1)?)),
            "$size" => {
                let Some(size) = operand.as_f64() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidOperatorPayload,
                        "$size expects a number",
                    ));
                };
                out.size = Some(size as usize);
            }
            "$elemMatch" => {
                out.elem_match = Some(Box::new(compile_elem_match(operand, depth + 1)?));
            }
            "$regex" => {
                let Some(pattern) = operand.as_str() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidRegex,
                        "$regex expects a string pattern",
                    ));
                };
                regex_pattern = Some(pattern.to_string());
            }
            "$options" => {
                let Some(text) = operand.as_str() else {
                    return Err(QueryError::new(
                        QueryErrorCode::InvalidRegex,
                        "$options expects a string",
                    ));
                };
                options = Some(text.to_string());
            }
            other => {
                return Err(QueryError::new(
                    QueryErrorCode::UnknownOperator,
                    format!("unknown query operator {other}"),
                ))
            }
        }
    }

    if let Some(pattern) = regex_pattern {
        let flags = match &options {
            Some(options) => RegexFlags::parse(options)?,
            None => RegexFlags::none(),
        };
        out.regex = Some(compile_regex(&pattern, flags)?);
    } else if options.is_some() {
        // mingo's `normalize` folds `$options` into a `$regex` in the same object;
        // a lone `$options` is not an operator its context knows, so it throws
        // `unknown query operator $options`. Verified against upstream 17.6.0.
        return Err(QueryError::new(
            QueryErrorCode::UnknownOperator,
            "unknown query operator $options",
        ));
    }

    Ok(out)
}

fn compile_elem_match(value: &Value, depth: usize) -> Result<CompiledElemMatch, QueryError> {
    let Some(object) = value.as_object() else {
        return Err(QueryError::new(
            QueryErrorCode::InvalidOperatorPayload,
            "$elemMatch expects an object",
        ));
    };
    if object.is_empty() {
        return Ok(CompiledElemMatch::OnlyObjects);
    }
    let all_value_operators = object
        .keys()
        .all(|key| is_operator(key) && !matches!(key.as_str(), "$and" | "$or" | "$nor"));
    if all_value_operators {
        Ok(CompiledElemMatch::AsValue(compile_field_value_inner(
            value,
            depth + 1,
        )?))
    } else {
        Ok(CompiledElemMatch::AsSelector(compile_selector(
            value,
            depth + 1,
        )?))
    }
}

impl CompiledSelector {
    fn test(&self, document: &Value) -> bool {
        if let Some(field) = &self.field {
            return field.test(None, MINGO_DEFAULT_DEPTH);
        }
        for field in &self.fields {
            let resolved = resolve(document, &field.path);
            if !field
                .value
                .test_resolved(&resolved, path_depth(&field.path))
            {
                return false;
            }
        }
        // `$and`: `queries.every(...)`, so an empty array is true.
        if !self.and.iter().all(|arm| arm.test(document)) {
            return false;
        }
        // `$or`: `queries.some(...)`, so an empty array is false — but only when
        // the key was present. An absent `$or` must not exclude anything, which is
        // what `any_or` distinguishes.
        if self.any_or && !self.or.iter().any(|arm| arm.test(document)) {
            return false;
        }
        // `$nor`: `!$or(...)`, so an empty array is true.
        if self.nor.iter().any(|arm| arm.test(document)) {
            return false;
        }
        true
    }
}

impl CompiledFieldValue {
    /// Tests a plain optional value, for callers that already resolved one.
    fn test(&self, value: Option<&Value>, depth: usize) -> bool {
        let resolved = match value {
            // An explicit `null` in a document is a present value, so it must not
            // be conflated with an absent path.
            Some(value) => Resolved::Value(std::borrow::Cow::Borrowed(value)),
            None => Resolved::Absent,
        };
        self.test_resolved(&resolved, depth)
    }

    /// Tests a resolved path. `depth` is the path's separator count, which `$eq`
    /// uses for array flattening.
    fn test_resolved(&self, resolved: &Resolved<'_>, depth: usize) -> bool {
        let owned = resolved.value();
        let value: Option<&Value> = owned.as_deref();
        if let Some(expected) = &self.eq {
            if !eq(value, expected, depth) {
                return false;
            }
        }
        if let Some(expected) = &self.ne {
            if eq(value, expected, depth) {
                return false;
            }
        }
        if let Some(expected) = &self.gt {
            if !compare_with(value, expected, Comparison::Gt) {
                return false;
            }
        }
        if let Some(expected) = &self.gte {
            if !compare_with(value, expected, Comparison::Gte) {
                return false;
            }
        }
        if let Some(expected) = &self.lt {
            if !compare_with(value, expected, Comparison::Lt) {
                return false;
            }
        }
        if let Some(expected) = &self.lte {
            if !compare_with(value, expected, Comparison::Lte) {
                return false;
            }
        }
        if let Some(values) = &self.in_values {
            if !in_operator(value, values) {
                return false;
            }
        }
        if let Some(values) = &self.nin_values {
            if in_operator(value, values) {
                return false;
            }
        }
        if let Some(flag) = self.exists {
            // `$exists` is the one operator that distinguishes "the key is absent"
            // from "the value is null", and for a path that mapped over an array it
            // asks whether *any* element resolved. `Resolved::exists` carries both
            // rules.
            if resolved.exists() != flag {
                return false;
            }
        }
        if let Some(names) = &self.type_names {
            let matched = names.iter().any(|name| matches_type(value, name));
            if !matched {
                return false;
            }
        }
        if let Some((divisor, remainder)) = self.modulus {
            if !modulus_matches(value, divisor, remainder) {
                return false;
            }
        }
        if let Some(size) = self.size {
            let matched = value
                .and_then(Value::as_array)
                .map(|array| array.len() == size)
                .unwrap_or(false);
            if !matched {
                return false;
            }
        }
        if let Some(elem_match) = &self.elem_match {
            if !elem_match_matches(value, elem_match) {
                return false;
            }
        }
        if let Some(regex) = &self.regex {
            if !regex_matches(value, regex) {
                return false;
            }
        }
        if let Some(inner) = &self.not {
            if inner.test(value, depth) {
                return false;
            }
        }
        true
    }
}

/// The four ordering comparisons, so [`compare_with`] can be one function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Gt,
    Gte,
    Lt,
    Lte,
}

fn compare_with(value: Option<&Value>, expected: &Value, comparison: Comparison) -> bool {
    // `compare(a, b, f)` in `_predicates.js`: "for each element of ensureArray(a),
    // if typeOf(v) === typeOf(b) and f(v, b) then true". So a document-side array
    // matches when *one* of its elements matches, and a type mismatch is false
    // rather than an ordering.
    let candidates: Vec<Option<&Value>> = match value {
        Some(Value::Array(items)) => items.iter().map(Some).collect(),
        other => vec![other],
    };
    candidates.into_iter().any(|candidate| {
        let Some(candidate) = candidate else {
            return false;
        };
        if mingo_type_of(candidate) != mingo_type_of(expected) {
            return false;
        }
        let ordering = mingo_cmp(candidate, expected);
        match comparison {
            Comparison::Gt => ordering == std::cmp::Ordering::Greater,
            Comparison::Gte => ordering != std::cmp::Ordering::Less,
            Comparison::Lt => ordering == std::cmp::Ordering::Less,
            Comparison::Lte => ordering != std::cmp::Ordering::Greater,
        }
    })
}

/// `$eq` from `_predicates.js`.
fn eq(value: Option<&Value>, expected: &Value, depth: usize) -> bool {
    let lhs = value;
    // `isNil(a) && isNil(b)` — a missing field equals an explicit null.
    let lhs_nil = lhs.map(Value::is_null).unwrap_or(true);
    let rhs_nil = expected.is_null();
    if lhs_nil && rhs_nil {
        return true;
    }
    if let (Some(lhs), Some(rhs)) = (lhs, Some(expected)) {
        if is_equal(lhs, rhs) {
            return true;
        }
        if let Value::Array(items) = lhs {
            // `a.some(v => isEqual(v, b)) || flatten(a, depth).some(...)`
            if items.iter().any(|item| is_equal(item, rhs)) {
                return true;
            }
            let flattened = flatten(items, depth);
            if flattened.iter().any(|item| is_equal(item, rhs)) {
                return true;
            }
        }
    }
    false
}

/// `$in` from `_predicates.js`.
fn in_operator(value: Option<&Value>, expected: &[Value]) -> bool {
    // `if (isNil(a)) return b.some(v => v === null)` — a missing field matches
    // `$in: [null]` and nothing else.
    let Some(value) = value else {
        return expected.iter().any(Value::is_null);
    };
    let values: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    // `intersection([values, b]).length > 0`, where intersection is by structural
    // equality, not by identity.
    if values
        .iter()
        .any(|candidate| expected.iter().any(|e| is_equal(candidate, e)))
    {
        return true;
    }
    false
}

fn modulus_matches(value: Option<&Value>, divisor: f64, remainder: f64) -> bool {
    let candidates: Vec<&Value> = match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    };
    candidates.iter().any(|candidate| match candidate {
        Value::Number(n) => match n.as_f64() {
            // JavaScript's `%` is a remainder with the sign of the dividend, and
            // Rust's `%` for f64 has the same sign rule, so they agree.
            Some(x) => x % divisor == remainder,
            None => false,
        },
        _ => false,
    })
}

fn elem_match_matches(value: Option<&Value>, elem_match: &CompiledElemMatch) -> bool {
    let Some(Value::Array(items)) = value else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    match elem_match {
        CompiledElemMatch::OnlyObjects => items.iter().any(|item| item.is_object()),
        CompiledElemMatch::AsValue(field) => items
            .iter()
            .any(|item| field.test(Some(item), MINGO_DEFAULT_DEPTH)),
        CompiledElemMatch::AsSelector(selector) => {
            items.iter().any(|item| selector.test(item))
        }
    }
}

/// `$regex`: `ensureArray(a).some(x => isString(x) && b.exec(x))`, plus a
/// one-level flatten so a regex matches inside a nested array of strings too.
fn regex_matches(value: Option<&Value>, regex: &RegexMatcher) -> bool {
    let Some(value) = value else {
        return false;
    };
    let items: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    if items
        .iter()
        .any(|item| item.as_str().map(|s| regex.is_match(s)).unwrap_or(false))
    {
        return true;
    }
    // `flatten(lhs, 1).some(match)`
    items
        .iter()
        .filter_map(|item| item.as_array())
        .flatten()
        .any(|item| item.as_str().map(|s| regex.is_match(s)).unwrap_or(false))
}

/// `$type` type names, from `compareFuncs` in `_predicates.js`.
///
/// The map includes both mingo's own type vocabulary and MongoDB's numeric aliases
/// (`1` is `double`, `16` is `int`, ...), because upstream passes whatever the
/// selector holds straight to `compareFuncs`.
fn matches_type(value: Option<&Value>, name: &str) -> bool {
    let is_nil = value.map(Value::is_null).unwrap_or(true);
    match name {
        "array" => value.map(Value::is_array).unwrap_or(false),
        "boolean" | "bool" => value.map(Value::is_boolean).unwrap_or(false),
        "number" => value
            .and_then(Value::as_f64)
            .map(|v| !v.is_nan())
            .unwrap_or(false),
        "int" | "long" => value
            .and_then(Value::as_f64)
            .map(|v| v.is_finite() && v.fract() == 0.0)
            .unwrap_or(false),
        "double" | "decimal" => value.map(Value::is_number).unwrap_or(false),
        // `null` is `a === null` (`compareFuncs.null` in `_predicates.js`), *not* `isNil`:
        // a missing field is not of type null. Verified against upstream 17.6.0 — the
        // `selector-matching` oracle's `type-null` case matches only the documents whose
        // `firstName` is explicitly null, not the ones lacking the field.
        "null" => value.map(Value::is_null).unwrap_or(false),
        // `undefined: isNil`, by contrast, so a missing field *and* an explicit null both
        // satisfy `$type: 'undefined'`. That asymmetry is upstream's.
        "undefined" => is_nil,
        "object" => value.map(Value::is_object).unwrap_or(false),
        "string" => value.map(Value::is_string).unwrap_or(false),
        // Mongo identifiers that map onto the same predicates.
        "1" => value.map(Value::is_number).unwrap_or(false),
        "2" => value.map(Value::is_string).unwrap_or(false),
        "3" => value.map(Value::is_object).unwrap_or(false),
        "4" => value.map(Value::is_array).unwrap_or(false),
        // `6` is deprecated `undefined` (isNil); `10` is `null` (strict).
        "6" => is_nil,
        "10" => value.map(Value::is_null).unwrap_or(false),
        "8" => value.map(Value::is_boolean).unwrap_or(false),
        "16" | "18" => value
            .and_then(Value::as_f64)
            .map(|v| v.is_finite() && v.fract() == 0.0)
            .unwrap_or(false),
        "19" => value.map(Value::is_number).unwrap_or(false),
        // `date`, `regexp`, `symbol`, `function` and `arraybuffer` have no JSON
        // representation, so no document can hold one. They match nothing, which is
        // also what mingo's `compareFuncs[b] ? f(a) : false` does when the name is
        // unknown.
        _ => false,
    }
}

/// mingo's `typeOf` (`util/_internal.js`). JSON restricts the universe to
/// `null`, `boolean`, `number`, `string`, `array` and `object`, so the JS-only
/// names collapse onto "no JSON value can be this", answered by
/// [`matches_type`].
fn mingo_type_of(value: &Value) -> MingoType {
    match value {
        Value::Null => MingoType::Null,
        Value::Bool(_) => MingoType::Boolean,
        Value::Number(_) => MingoType::Number,
        Value::String(_) => MingoType::String,
        Value::Array(_) => MingoType::Array,
        Value::Object(_) => MingoType::Object,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MingoType {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

/// The cross-type sort order from `SORT_ORDER` in `util/_internal.js`.
///
/// Ordering comparisons never reach this (they require equal types), so it exists
/// for completeness and for the sort comparator's object case.
fn mingo_type_rank(value_type: MingoType) -> u8 {
    match value_type {
        MingoType::Null => 2,
        MingoType::Number => 3,
        MingoType::String => 4,
        MingoType::Object => 6,
        MingoType::Array => 7,
        MingoType::Boolean => 9,
    }
}

/// mingo's `mingoCmp` for the JSON-representable types.
///
/// The important differences from JavaScript's `<` and from `Value`'s own
/// comparison: strings compare by UTF-16 code unit (via [`dxdb_jsstr`], PLAN.md
/// 3.2), arrays compare element-wise *after sorting*, and objects compare by
/// sorted key list and then value-by-value.
pub fn mingo_compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    mingo_cmp(a, b)
}

fn mingo_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let type_a = mingo_type_of(a);
    let type_b = mingo_type_of(b);
    if type_a == type_b {
        match type_a {
            MingoType::Null => return Ordering::Equal,
            MingoType::Boolean => {
                let (x, y) = (a.as_bool().unwrap_or(false), b.as_bool().unwrap_or(false));
                return x.cmp(&y);
            }
            MingoType::Number => {
                let (x, y) = (
                    a.as_f64().unwrap_or_default(),
                    b.as_f64().unwrap_or_default(),
                );
                return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            }
            MingoType::String => {
                return dxdb_jsstr::compare_js(
                    a.as_str().unwrap_or_default(),
                    b.as_str().unwrap_or_default(),
                );
            }
            MingoType::Array => {
                let xs = a.as_array().cloned().unwrap_or_default();
                let ys = b.as_array().cloned().unwrap_or_default();
                // `a.slice().sort(mingoCmp)` — arrays compare as sorted multisets.
                let mut xs = xs;
                let mut ys = ys;
                xs.sort_by(mingo_cmp);
                ys.sort_by(mingo_cmp);
                for (x, y) in xs.iter().zip(ys.iter()) {
                    let order = mingo_cmp(x, y);
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                return xs.len().cmp(&ys.len());
            }
            MingoType::Object => {
                let empty = Map::new();
                let xs = a.as_object().unwrap_or(&empty);
                let mut keys_a: Vec<&String> = xs.keys().collect();
                keys_a.sort_by(|x, y| dxdb_jsstr::compare_js(x, y));
                let ysobj = b.as_object().unwrap_or(&empty);
                let mut keys_b: Vec<&String> = ysobj.keys().collect();
                keys_b.sort_by(|x, y| dxdb_jsstr::compare_js(x, y));
                for (x, y) in keys_a.iter().zip(keys_b.iter()) {
                    let order = dxdb_jsstr::compare_js(x, y);
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                let order = keys_a.len().cmp(&keys_b.len());
                if order != Ordering::Equal {
                    return order;
                }
                for key in keys_a {
                    let order = mingo_cmp(&xs[key], &ysobj[key]);
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                return Ordering::Equal;
            }
        }
    }
    mingo_type_rank(type_a).cmp(&mingo_type_rank(type_b))
}

/// mingo's `isEqual`: structural equality, key order irrelevant, same type.
pub fn is_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        // `Object.is(a, b)` first, which for numbers is `===` except that
        // `Object.is(NaN, NaN)` is true; JSON has no NaN, so `==` is the same.
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| is_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).map(|other| is_equal(value, other)).unwrap_or(false))
        }
        _ => false,
    }
}

/// JavaScript truthiness, used only where mingo tests a selector operand as a
/// boolean (`$exists: !!value`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        // The empty string is falsy; so is every other falsy primitive, but a
        // non-empty string is truthy.
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// mingo's `flatten(xs, depth)`: descend into nested arrays up to `depth` levels,
/// where a negative depth means "all the way".
fn flatten(items: &[Value], depth: usize) -> Vec<Value> {
    fn walk(items: &[Value], depth: i64, out: &mut Vec<Value>) {
        for item in items {
            match item {
                Value::Array(inner) if depth != 0 => {
                    let next = if depth < 0 {
                        depth
                    } else {
                        (depth - 1).max(-1)
                    };
                    walk(inner, next, out);
                }
                other => out.push(other.clone()),
            }
        }
    }
    let mut out = Vec::new();
    walk(items, depth as i64, &mut out);
    out
}

/// The number of separators in a path, which is mingo's `depth` for `$eq`'s
/// array flattening (`processQuery`: `while ((begin = selector.indexOf(".", begin
/// + 1)) !== -1) depth++`).
///
/// `ComputeOptions::update` writes this into the locals object that `$eq` reads
/// as `options.local.depth`, so a dotted path really does flatten deeper than a
/// bare one. Measured against upstream 17.6.0 rather than read off the source.
fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'.').count()
}

/// The outcome of resolving a field path.
///
/// The distinction between [`Resolved::Absent`] and
/// [`Resolved::Mapped`] exists because mingo's `$exists` treats them
/// differently: a path that mapped over an array of objects collects only the
/// values it found, so an empty result means "no element had that property",
/// which is absent — while a `$size` on the same empty array is still a real
/// (empty) array.
#[derive(Debug, Clone)]
pub enum Resolved<'a> {
    /// The path produced no value.
    Absent,
    /// The path produced a value taken from the document.
    Value(std::borrow::Cow<'a, Value>),
    /// The path descended through an array of objects and collected the values
    /// found, in element order.
    Mapped(Vec<Value>),
}

impl<'a> Resolved<'a> {
    /// The resolved value, whichever shape it came in. `None` when absent.
    pub fn value(&self) -> Option<std::borrow::Cow<'a, Value>> {
        match self {
            Self::Absent => None,
            Self::Value(value) => Some(value.clone()),
            // A mapped result is a fresh array, as it is in mingo.
            Self::Mapped(items) => Some(std::borrow::Cow::Owned(Value::Array(items.clone()))),
        }
    }

    /// Whether `$exists: true` should hold.
    ///
    /// `resolve(o, selector) !== undefined` for a plain path; for the array case
    /// mingo checks `val.some(v => v !== undefined)`, which is false for an empty
    /// collection even though the collection itself exists.
    pub fn exists(&self) -> bool {
        match self {
            Self::Absent => false,
            Self::Mapped(items) => !items.is_empty(),
            Self::Value(_) => true,
        }
    }
}

/// Resolves a field path against a document, ported from mingo's
/// `resolve(o, path, {unwrapArray: true})` in `esm/util/_internal.js`.
///
/// Three behaviours are load-bearing and each is covered by a vector or a ported
/// test:
///
/// 1. A dotless path returns the property as it is, array included. An array is
///    *not* descended for path resolution.
/// 2. A non-numeric segment applied to an array maps over the elements, collects
///    the values that resolved, and stops. That is why `{'a.b': 1}` matches
///    `{a: [{b: 1}, {b: 2}]}`.
/// 3. A one-element array of arrays is unwrapped (`unwrap`), which is why a
///    doubly-nested array needs the mapping rather than the unwrap to match.
pub fn resolve<'a>(document: &'a Value, path: &str) -> Resolved<'a> {
    // `if (isScalar(obj)) return obj` — a scalar document is its own resolution.
    if !matches!(document, Value::Array(_) | Value::Object(_)) {
        return Resolved::Value(std::borrow::Cow::Borrowed(document));
    }
    // `if (!selector.includes(".") && !isArray(obj)) return getValue(obj, selector)`.
    if !path.contains('.') && !document.is_array() {
        return match document.get(path) {
            Some(value) => Resolved::Value(std::borrow::Cow::Borrowed(value)),
            None => Resolved::Absent,
        };
    }
    let mut depth = 0usize;
    resolve_path(document, path, &mut depth)
}

fn resolve_path<'a>(document: &'a Value, path: &str, depth: &mut usize) -> Resolved<'a> {
    let bytes = path.as_bytes();
    let mut begin = 0usize;
    let mut current: &'a Value = document;
    loop {
        let dot = bytes[begin.min(bytes.len())..]
            .iter()
            .position(|byte| *byte == b'.')
            .map(|offset| begin + offset);
        let field = match dot {
            Some(dot) => &path[begin..dot],
            None => &path[begin..],
        };
        let is_index = !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit());
        if !is_index && current.is_array() {
            // mingo's guard: `if (begin === 0 && depth > 0) break;` — a top-level
            // array whose path starts with a non-index segment gives up rather
            // than mapping, which keeps `resolve` from looping.
            if begin == 0 && *depth > 0 {
                return Resolved::Absent;
            }
            *depth += 1;
            let subpath = &path[begin..];
            let items = current.as_array().cloned().unwrap_or_default();
            let mut collected = Vec::with_capacity(items.len());
            for item in &items {
                let mut inner_depth = *depth;
                let resolved = resolve_path_from(item, subpath, &mut inner_depth);
                if let Some(value) = resolved {
                    collected.push(value);
                }
            }
            return Resolved::Mapped(collected);
        }
        let next = match current.get(field) {
            Some(value) => value,
            None => return Resolved::Absent,
        };
        match dot {
            None => return Resolved::Value(std::borrow::Cow::Borrowed(next)),
            Some(dot) => {
                current = next;
                begin = dot + 1;
            }
        }
    }
}

/// Resolves `path` from `value` for the array-mapping branch, where the result is
/// owned rather than borrowed.
fn resolve_path_from(value: &Value, path: &str, depth: &mut usize) -> Option<Value> {
    let mut current = value;
    let mut begin = 0usize;
    let bytes = path.as_bytes();
    loop {
        let dot = bytes[begin.min(bytes.len())..]
            .iter()
            .position(|byte| *byte == b'.')
            .map(|offset| begin + offset);
        let field = match dot {
            Some(dot) => &path[begin..dot],
            None => &path[begin..],
        };
        let is_index = !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit());
        if !is_index && current.is_array() {
            if begin == 0 && *depth > 0 {
                return None;
            }
            *depth += 1;
            let subpath = &path[begin..];
            let items = current.as_array().cloned().unwrap_or_default();
            let mut collected = Vec::with_capacity(items.len());
            for item in &items {
                if let Some(resolved) = resolve_path_from(item, subpath, depth) {
                    collected.push(resolved);
                }
            }
            return Some(Value::Array(collected));
        }
        let next = current.get(field)?;
        match dot {
            None => return Some(next.clone()),
            Some(dot) => {
                current = next;
                begin = dot + 1;
            }
        }
    }
}

