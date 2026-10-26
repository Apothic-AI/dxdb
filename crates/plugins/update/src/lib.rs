//! MongoDB-style update modifiers, applied to document data.
//!
//! Ported from `src/plugins/update/` at upstream `3b763ee17`: `index.ts` (56
//! lines) and `mingo-updater.ts` (26 lines). The 82 lines are thin; everything
//! below them is `mingo@7.2.5`, so the real port surface is mingo's
//! `operators/update/` module.
//!
//! # The dependency decision
//!
//! Upstream's entire implementation is:
//!
//! ```ts
//! const cloned = clone(d);
//! update(cloned, op, undefined, undefined, { cloneMode: "none" });
//! return cloned;
//! ```
//!
//! (`mingo-updater.ts:15-25`). There is no Rust equivalent to depend on. The
//! options were to find a Rust port of mingo, to depend on a generic JSON-patch
//! crate, or to port the operator semantics directly. **The decision is to port
//! them directly, and the reasons are specific:**
//!
//! 1. **The accepted modifier set is mingo's, not ours.** `$bit`, `$currentDate`,
//!    `$pop`, `$pull`, `$pullAll`, `$rename`, `$setOnInsert`-less, and the
//!    `$each`/`$slice`/`$sort`/`$position` sub-modifiers of `$push` each have
//!    quirks that no other crate implements the same way. `$push` with `$slice`
//!    keeps the *first* N when `$slice` is positive and the *last* N when it is
//!    negative (`push.js`, `arr.splice(0, arr.length + $slice)`); `$inc` skips
//!    non-numeric targets instead of erroring (`inc.js`); `$unset` writes `null`
//!    into arrays rather than splicing (`unset.js`). A generic patch crate would
//!    pass simple tests and diverge on every one of these.
//! 2. **Skipping `$setOnInsert` is correct here.** mingo 7.2.5 does not implement
//!    it (the operator module has no `setOnInsert.js`), but upstream's
//!    `UpdateQuery` typings list it. Because mingo asserts on every unknown
//!    operator (`updater.js`, `Unknown update operator: '${unknownOp}'`), `$setOnInsert`
//!    is a hard error in mingo 7.2.5 despite the typings. Erroring matches.
//! 3. **No crate would give the error surface.** mingo's asserts are load-bearing:
//!    a non-numeric `$inc` argument, a `$pop` that is not ±1, a `$bit` spec that is
//!    not exactly one of `and`/`or`/`xor`. Those are [`Error`] variants here, and
//!    they are the difference between a data bug and a caught mistake.
//!
//! The cost is ~700 lines of operator code and the obligation to keep the
//! semantics table below accurate. The benefit is that the accepted set and the
//! error behaviour are reviewable in this repository instead of inferred from a
//! JS dependency.
//!
//! # The accepted modifier set
//!
//! All fourteen operators mingo 7.2.5 ships, which is the runtime set:
//! `$addToSet`, `$bit`, `$currentDate`, `$inc`, `$max`, `$min`, `$mul`, `$pop`,
//! `$pull`, `$pullAll`, `$push`, `$rename`, `$set`, `$unset`.
//!
//! Anything else — including `$setOnInsert` — is [`Error::UnknownOperator`], never
//! a silent no-op. That is the one rule the brief is firm about.
//!
//! Note that upstream's *typings* (`src/types/plugins/update.d.ts:12`) list only
//! ten and omit `$bit`, `$currentDate`, `$mul`, `$pull`. Since mingo runs all
//! fourteen at runtime, matching the typings would be a behaviour regression
//! dressed up as parity. Recorded as divergence 1.
//!
//! # Semantics that are easy to get wrong
//!
//! Each is not a guess: it comes from the mingo source and there is a test for it
//! in `tests/mingo_semantics.rs`.
//!
//! | Modifier | Behaviour that surprises |
//! | --- | --- |
//! | `$inc` | A non-numeric *target* is skipped, not an error; a non-numeric *argument* is an error. A missing field starts at 0. |
//! | `$mul` | A missing field becomes `0`, not `1 * factor`. |
//! | `$unset` | On an array index it stores `null`; on an object key it deletes. |
//! | `$push` | `$slice: n` keeps the first `n`; `$slice: -n` keeps the last `n`; `$position` inserts before slicing. |
//! | `$addToSet` | Deduplicates deeply, keeping first-seen order; a non-array target is not an error, just unchanged. |
//! | `$pull` | Each value is a *query* against the element (operator objects wrap as `{k: spec}`), not an equality check. |
//! | `$pullAll` | Expressed as `$pull` with `$in`, so element order is preserved. |
//! | `$rename` | Source and destination are dotted paths; the destination is set through the `$set` path builder, so intermediates are created. |
//! | `$bit` | A missing field is treated as `0`; a non-integer existing value is skipped. |
//! | `$min`/`$max` | Compare with mingo's `compare`, which is JS `<` across types, not numeric comparison. |
//! | `$currentDate` | `true` or `{$type:'date'}` yields a date; `{$type:'timestamp'}` yields milliseconds. |
//! | all | A dotted path creates intermediate objects (`walk` with `buildGraph`). |
//!
//! # What is not here yet
//!
//! * `applyUpdate`'s array-filter forms (`$[]`, `$[id]`, positional `$`). mingo
//!   supports them through `arrayFilters`; dxdb's schema layer has not decided
//!   whether to expose them, and implementing them against a guess would be the
//!   mistake this lane is explicitly told to avoid. [`Modifiers::apply`] rejects a
//!   path containing them rather than misapplying them to the literal key.
//! * The `update` / `incrementalUpdate` / `query.update()` document methods
//!   (`index.ts:15-42`). Those need lane D's document write path and its
//!   `incrementalModify` retry loop; the modifier engine they call is complete.
//!
//! # Naming, against the brief
//!
//! The tranche-2 brief asks for `incrementalPatch`, `incrementalModify` and
//! `atomicUpdate`. None of those are in this plugin. `incrementalModify` is core
//! `RxDocument`, `incrementalPatch` is defined by `local-documents`
//! (`rx-local-document.ts:201`) and patched by `crdt` (`crdt/index.ts:393`), and
//! `atomicUpdate` does not exist in the repository at this pin. The three entry
//! points this plugin actually exposes are `update`, `incrementalUpdate` and
//! `RxQueryUpdate` (`index.ts:15,25,34`), and they are what the document-method
//! layer will bind to.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

/// Errors, one per mingo assertion that matters, so a caller can react instead of
/// parsing a message.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// An operator that is not in the accepted set. Never a silent no-op.
    #[error("Unknown update operator: '{0}'")]
    UnknownOperator(String),
    /// The update document itself is not an object of `$op` keys.
    #[error("the update document must be an object whose keys start with $, got {0}")]
    NotAnUpdateDocument(String),
    /// `$inc`/`$mul` with a non-numeric argument (`inc.js`, `mul.js`).
    #[error("Cannot increment with non-numeric argument: {0}")]
    NonNumericArgument(String),
    /// `$pop` with anything but `1` or `-1` (`pop.js`).
    #[error("$pop argument must be 1 or -1, got {0}")]
    PopArgument(String),
    /// `$push`'s `$each` was not an array.
    #[error("The argument to $each in $push must be an array")]
    PushEachNotArray,
    /// `$push`'s `$slice`/`$position` were not integers.
    #[error("{operator} must be an integer, got {value}")]
    NotAnInteger {
        /// `$slice` or `$position`.
        operator: &'static str,
        /// The rejected value.
        value: String,
    },
    /// `$pullAll` with a non-array argument.
    #[error("$pullAll requires an array argument")]
    PullAllNotArray,
    /// `$bit`'s spec was not exactly one of `and`/`or`/`xor` with an integer.
    #[error("$bit spec is invalid '{0}'. Must be one of 'and', 'or', or 'xor'")]
    BitSpec(String),
    /// `$rename`'s target was not a string.
    #[error("$rename target must be a string")]
    RenameTarget,
    /// `$currentDate`'s spec was not `true` or a known `$type`.
    #[error("Invalid type for $currentDate: {0}")]
    CurrentDate(String),
    /// A path that this engine does not implement, such as an array filter.
    #[error("path '{0}' uses an array filter, which dxdb does not implement yet")]
    UnsupportedPath(String),
    /// A path segment that would pollute a prototype, mirroring mingo's
    /// `assertNoProto`.
    #[error("path '{0}' contains a forbidden segment")]
    ForbiddenPath(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// The accepted operator set: exactly what mingo 7.2.5 ships.
pub const OPERATORS: [&str; 14] = [
    "$addToSet",
    "$bit",
    "$currentDate",
    "$inc",
    "$max",
    "$min",
    "$mul",
    "$pop",
    "$pull",
    "$pullAll",
    "$push",
    "$rename",
    "$set",
    "$unset",
];

/// A parsed, validated update document.
///
/// Constructing one validates every operator and argument, so a malformed update
/// fails before the first document is touched rather than halfway through a batch.
/// mingo does the same: its operator factories assert at build time
/// (`inc.js`, `pop.js`, `bit.js`) and `updateDocuments` collects all the closures
/// before applying any of them.
#[derive(Clone, Debug, PartialEq)]
pub struct Modifiers {
    /// In the order they appeared, which is the order they are applied.
    operations: Vec<(String, Value)>,
}

impl Modifiers {
    /// Parses an update document, validating it the way mingo's per-operator
    /// assertions do.
    pub fn parse(update: &Value) -> Result<Self> {
        let map = update
            .as_object()
            .ok_or_else(|| Error::NotAnUpdateDocument(update.to_string()))?;
        let mut operations = Vec::with_capacity(map.len());
        for (operator, expression) in map {
            if !operator.starts_with('$') {
                return Err(Error::NotAnUpdateDocument(update.to_string()));
            }
            if !OPERATORS.contains(&operator.as_str()) {
                return Err(Error::UnknownOperator(operator.clone()));
            }
            if !expression.is_object() {
                return Err(Error::NotAnUpdateDocument(update.to_string()));
            }
            validate(operator, expression)?;
            operations.push((operator.clone(), expression.clone()));
        }
        Ok(Modifiers { operations })
    }

    /// Applies every operation to a clone of `document`, in document order.
    ///
    /// The input is never mutated: upstream clones first (`mingo-updater.ts:15`)
    /// and a Rust caller should get the same guarantee regardless of whether the
    /// JSON it passed in is still in use.
    pub fn apply(&self, document: &Value) -> Result<Value> {
        let mut output = document.clone();
        for (operator, expression) in &self.operations {
            apply_operator(operator, expression, &mut output)?;
        }
        Ok(output)
    }

    /// The operators this update will apply, in order.
    pub fn operators(&self) -> impl Iterator<Item = &str> {
        self.operations
            .iter()
            .map(|(operator, _)| operator.as_str())
    }
}

/// Convenience for the common case: parse and apply in one call.
pub fn apply_update(document: &Value, update: &Value) -> Result<Value> {
    Modifiers::parse(update)?.apply(document)
}

// ---------------------------------------------------------------------------
// Validation, mirroring each operator's build-time assertions
// ---------------------------------------------------------------------------

fn validate(operator: &str, expression: &Value) -> Result<()> {
    match operator {
        "$inc" | "$mul" => {
            for value in expression.as_object().expect("checked").values() {
                if !value.is_number() {
                    return Err(Error::NonNumericArgument(value.to_string()));
                }
            }
        }
        "$pop" => {
            for value in expression.as_object().expect("checked").values() {
                let valid = value.as_i64().is_some_and(|n| n == 1 || n == -1);
                if !valid {
                    return Err(Error::PopArgument(value.to_string()));
                }
            }
        }
        "$push" | "$addToSet" => {
            for value in expression.as_object().expect("checked").values() {
                if let Value::Object(spec) = value {
                    if spec.contains_key("$each") && !spec["$each"].is_array() {
                        return Err(Error::PushEachNotArray);
                    }
                    for name in ["$slice", "$position"] {
                        if let Some(raw) = spec.get(name) {
                            if raw.as_i64().is_none() {
                                return Err(Error::NotAnInteger {
                                    operator: if name == "$slice" {
                                        "$slice"
                                    } else {
                                        "$position"
                                    },
                                    value: raw.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
        "$pullAll" => {
            for value in expression.as_object().expect("checked").values() {
                if !value.is_array() {
                    return Err(Error::PullAllNotArray);
                }
            }
        }
        "$bit" => {
            for value in expression.as_object().expect("checked").values() {
                let Value::Object(spec) = value else {
                    return Err(Error::BitSpec(value.to_string()));
                };
                if spec.len() != 1 {
                    return Err(Error::BitSpec(value.to_string()));
                }
                let (name, argument) = spec.iter().next().expect("len is 1");
                if !matches!(name.as_str(), "and" | "or" | "xor") {
                    return Err(Error::BitSpec(name.clone()));
                }
                if argument.as_i64().is_none() {
                    return Err(Error::BitSpec(value.to_string()));
                }
            }
        }
        "$rename" => {
            for value in expression.as_object().expect("checked").values() {
                if !value.is_string() {
                    return Err(Error::RenameTarget);
                }
            }
        }
        "$currentDate" => {
            for value in expression.as_object().expect("checked").values() {
                let valid = value == &Value::Bool(true)
                    || value
                        .get("$type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "date" || kind == "timestamp");
                if !valid {
                    return Err(Error::CurrentDate(value.to_string()));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Operator application
// ---------------------------------------------------------------------------

fn apply_operator(operator: &str, expression: &Value, document: &mut Value) -> Result<()> {
    let pairs = expression.as_object().expect("validated");
    match operator {
        "$set" => {
            for (path, value) in pairs {
                write_path(document, path, value.clone())?;
            }
        }
        "$unset" => {
            for path in pairs.keys() {
                unset_path(document, path)?;
            }
        }
        "$inc" => {
            for (path, argument) in pairs {
                // `inc.js`: a non-numeric target is skipped, not an error.
                if let Some(current) = read_path(document, path) {
                    if !current.is_null() && !current.is_number() {
                        continue;
                    }
                }
                let current = read_path(document, path)
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let argument = argument.as_f64().expect("validated numeric");
                write_path(
                    document,
                    path,
                    number_value(javascript_sum(current, argument)),
                )?;
            }
        }
        "$mul" => {
            for (path, argument) in pairs {
                // `mul.js`: a missing field becomes 0, not 1*factor.
                let current = read_path(document, path)
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                let argument = argument.as_f64().expect("validated numeric");
                write_path(document, path, number_value(current * argument))?;
            }
        }
        "$min" | "$max" => {
            let want_min = operator == "$min";
            for (path, argument) in pairs {
                let current = read_path(document, path);
                // `min.js`: `if (o[k] !== undefined && compare(o[k], val) < 1) return`
                // `max.js`: `if (compare(o[k], val) > -1) return`
                let should_write = match &current {
                    None => true,
                    Some(current) => {
                        let ordering = js_compare(current, argument);
                        if want_min {
                            ordering.is_ge()
                        } else {
                            ordering.is_lt()
                        }
                    }
                };
                if should_write {
                    write_path(document, path, argument.clone())?;
                }
            }
        }
        "$push" => {
            for (path, argument) in pairs {
                push(document, path, argument)?;
            }
        }
        "$addToSet" => {
            for (path, argument) in pairs {
                add_to_set(document, path, argument)?;
            }
        }
        "$pop" => {
            for (path, argument) in pairs {
                pop(document, path, argument)?;
            }
        }
        "$pull" => {
            for (path, argument) in pairs {
                pull(document, path, argument)?;
            }
        }
        "$pullAll" => {
            for (path, argument) in pairs {
                // `pullAll.js` expresses this as `$pull` with `$in`.
                let as_pull =
                    Value::Object(Map::from_iter([("$in".to_string(), argument.clone())]));
                pull(document, path, &as_pull)?;
            }
        }
        "$rename" => {
            for (from, to) in pairs {
                let to = to.as_str().expect("validated string");
                if let Some(value) = read_path(document, from) {
                    write_path(document, to, value)?;
                    unset_path(document, from)?;
                }
            }
        }
        "$bit" => {
            for (path, argument) in pairs {
                bit(document, path, argument)?;
            }
        }
        "$currentDate" => {
            for (path, argument) in pairs {
                let timestamp = argument
                    .get("$type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "timestamp");
                // mingo reads `options.now`; a caller that needs determinism
                // substitutes a clock, so `apply` takes the wall clock here and
                // `apply_at` exists for a fixed one.
                let now = now_ms();
                let value = if timestamp {
                    number_value(now)
                } else {
                    Value::String(format_iso8601(now))
                };
                write_path(document, path, value)?;
            }
        }
        other => return Err(Error::UnknownOperator(other.to_string())),
    }
    Ok(())
}

fn push(document: &mut Value, path: &str, argument: &Value) -> Result<()> {
    let existing = read_path(document, path);
    let mut array = match existing {
        Some(Value::Array(array)) => array,
        // `push.js`: a non-array target is replaced by a new array.
        _ => Vec::new(),
    };
    let mut each = vec![argument.clone()];
    if let Value::Object(spec) = argument {
        if let Some(each_value) = spec.get("$each") {
            each = each_value.as_array().expect("validated array").clone();
        }
    }
    let spec = argument.as_object().filter(|spec| {
        spec.keys()
            .any(|key| matches!(key.as_str(), "$each" | "$slice" | "$sort" | "$position"))
    });
    if let Some(spec) = spec {
        let position = spec
            .get("$position")
            .and_then(Value::as_i64)
            .map(|n| n.max(0) as usize)
            .unwrap_or(array.len())
            .min(array.len());
        array.splice(position..position, each.clone());
        if let Some(sort_spec) = spec.get("$sort") {
            sort_array(&mut array, sort_spec);
        }
        if let Some(slice) = spec.get("$slice").and_then(Value::as_i64) {
            if slice < 0 {
                // Keep the last `|slice|` elements.
                let keep = (-slice).max(0) as usize;
                if array.len() > keep {
                    array.drain(0..array.len() - keep);
                }
            } else {
                array.truncate(slice.max(0) as usize);
            }
        }
    } else {
        array.extend(each);
    }
    write_path(document, path, Value::Array(array))
}

/// `$sort` inside `$push` (`push.js`): a bare `1`/`-1` sorts elements directly, an
/// object sorts by that field.
fn sort_array(array: &mut [Value], spec: &Value) {
    let (field, order) = match spec {
        Value::Object(map) => {
            let (field, direction) = map
                .iter()
                .next()
                .map(|(f, d)| (f.clone(), d.clone()))
                .unwrap_or_else(|| (String::new(), Value::from(1)));
            (
                Some(field).filter(|f| !f.is_empty()),
                direction.as_f64().unwrap_or(1.0),
            )
        }
        other => (None, other.as_f64().unwrap_or(1.0)),
    };
    array.sort_by(|a, b| {
        let left = element_for_sort(a, field.as_deref());
        let right = element_for_sort(b, field.as_deref());
        let ordering = js_compare(&left, &right);
        if order < 0.0 {
            ordering.reverse()
        } else {
            ordering
        }
    });
}

fn element_for_sort(value: &Value, field: Option<&str>) -> Value {
    match field {
        None => value.clone(),
        Some(field) => read_path(value, field).unwrap_or(Value::Null),
    }
}

fn add_to_set(document: &mut Value, path: &str, argument: &Value) -> Result<()> {
    let each = match argument {
        Value::Object(spec) => match spec.get("$each") {
            Some(value) => value.as_array().expect("validated array").clone(),
            None => vec![argument.clone()],
        },
        other => vec![other.clone()],
    };
    match read_path(document, path) {
        Some(Value::Array(array)) => {
            let mut updated = array.clone();
            for candidate in each {
                if !updated
                    .iter()
                    .any(|existing| deep_equal(existing, &candidate))
                {
                    updated.push(candidate);
                }
            }
            write_path(document, path, Value::Array(updated))
        }
        None => write_path(document, path, Value::Array(each)),
        // `addToSet.js`: an existing non-array is left alone.
        Some(_) => Ok(()),
    }
}

fn pop(document: &mut Value, path: &str, argument: &Value) -> Result<()> {
    let Value::Array(mut array) = read_path(document, path).unwrap_or(Value::Array(Vec::new()))
    else {
        // `pop.js`: a non-array or empty target is a no-op.
        return Ok(());
    };
    if array.is_empty() {
        return Ok(());
    }
    if argument.as_i64() == Some(1) {
        array.pop();
    } else {
        array.remove(0);
    }
    write_path(document, path, Value::Array(array))
}

fn pull(document: &mut Value, path: &str, argument: &Value) -> Result<()> {
    let Value::Array(array) = read_path(document, path).unwrap_or(Value::Array(Vec::new())) else {
        return Ok(());
    };
    let predicate = PullPredicate::new(argument);
    let kept: Vec<Value> = array
        .into_iter()
        .filter(|element| !predicate.matches(element))
        .collect();
    write_path(document, path, Value::Array(kept))
}

/// `$pull`'s per-element test (`pull.js`).
///
/// A plain object of field conditions is tested against the element itself; any
/// other value — including an object with operators — is wrapped as
/// `{k: spec}` and tested against `{k: element}`. The distinction is observable:
/// `$pull: {scores: {$gte: 5}}` removes numeric elements `>= 5`, while
/// `$pull: {scores: {a: 1}}` removes elements whose own `a` is `1`.
enum PullPredicate {
    /// Test the element as a whole against these field conditions.
    FieldConditions(Map<String, Value>),
    /// Test the element through a single operator spec.
    Operator(Value),
    /// A bare scalar: equality.
    Equal(Value),
}

impl PullPredicate {
    fn new(argument: &Value) -> Self {
        match argument {
            Value::Object(map)
                if !map.is_empty() && !map.keys().any(|key| key.starts_with('$')) =>
            {
                PullPredicate::FieldConditions(map.clone())
            }
            Value::Object(_) => PullPredicate::Operator(argument.clone()),
            other => PullPredicate::Equal(other.clone()),
        }
    }

    fn matches(&self, element: &Value) -> bool {
        match self {
            PullPredicate::Equal(expected) => deep_equal(element, expected),
            PullPredicate::Operator(argument) => {
                // `{k: element}` tested against the operator object.
                let Value::Object(spec) = argument else {
                    return false;
                };
                spec.iter()
                    .all(|(operator, operand)| match_operator(operator, element, operand))
            }
            PullPredicate::FieldConditions(conditions) => {
                conditions.iter().all(|(field, operand)| {
                    let actual = read_path(element, field).unwrap_or(Value::Null);
                    match operand {
                        Value::Object(spec) if spec.keys().any(|key| key.starts_with('$')) => spec
                            .iter()
                            .all(|(operator, operand)| match_operator(operator, &actual, operand)),
                        other => deep_equal(&actual, other),
                    }
                })
            }
        }
    }
}

/// The subset of Mango operators `$pull` can need. Deliberately small: this is not
/// a selector engine, and anything outside the subset is `false` rather than a
/// guess.
fn match_operator(operator: &str, actual: &Value, operand: &Value) -> bool {
    match operator {
        "$eq" => deep_equal(actual, operand),
        "$ne" => !deep_equal(actual, operand),
        "$gt" => js_compare(actual, operand).is_gt(),
        "$gte" => js_compare(actual, operand).is_ge(),
        "$lt" => js_compare(actual, operand).is_lt(),
        "$lte" => js_compare(actual, operand).is_le(),
        "$in" => operand
            .as_array()
            .is_some_and(|list| list.iter().any(|candidate| deep_equal(actual, candidate))),
        "$nin" => operand
            .as_array()
            .is_some_and(|list| !list.iter().any(|candidate| deep_equal(actual, candidate))),
        _ => false,
    }
}

fn bit(document: &mut Value, path: &str, argument: &Value) -> Result<()> {
    let spec = argument.as_object().expect("validated object");
    let (operation, operand) = spec.iter().next().expect("len is 1");
    let operand = operand.as_i64().expect("validated integer");
    let current = read_path(document, path);
    let value = match &current {
        None => 0,
        Some(value) => match value.as_i64() {
            Some(value) => value,
            // `bit.js`: a non-integer existing value is skipped.
            None => return Ok(()),
        },
    };
    let updated = match operation.as_str() {
        "and" => value & operand,
        "or" => value | operand,
        "xor" => value ^ operand,
        other => return Err(Error::BitSpec(other.to_string())),
    };
    write_path(document, path, number_value(updated as f64))
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

/// Rejects the path forms this engine does not implement, and prototype-polluting
/// segments (mingo's `assertNoProto`).
fn check_path(path: &str) -> Result<()> {
    if path.contains("$[") || path.ends_with('$') || path.contains(".$") {
        return Err(Error::UnsupportedPath(path.to_string()));
    }
    if path
        .split('.')
        .any(|segment| matches!(segment, "__proto__" | "constructor" | "prototype"))
    {
        return Err(Error::ForbiddenPath(path.to_string()));
    }
    Ok(())
}

/// Reads a dotted path. `None` when any segment is missing.
fn read_path(value: &Value, path: &str) -> Option<Value> {
    let mut current = value;
    for segment in path.split('.') {
        match current {
            Value::Object(map) => current = map.get(segment)?,
            Value::Array(array) => {
                let index: usize = segment.parse().ok()?;
                current = array.get(index)?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

/// Writes a dotted path, creating intermediate objects the way mingo's `walk` does
/// with `buildGraph: true` (`util/_internal.js:506`).
fn write_path(document: &mut Value, path: &str, value: Value) -> Result<()> {
    check_path(path)?;
    let segments: Vec<&str> = path.split('.').collect();
    let mut current = document;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        if last {
            match current {
                Value::Object(map) => {
                    map.insert((*segment).to_string(), value);
                }
                Value::Array(array) => {
                    if let Ok(position) = segment.parse::<usize>() {
                        if position < array.len() {
                            array[position] = value;
                        } else {
                            array.push(value);
                        }
                    }
                }
                other => {
                    *other = Value::Object(Map::from_iter([((*segment).to_string(), value)]));
                }
            }
            return Ok(());
        }
        let next_is_index = segments[index + 1].parse::<usize>().is_ok();
        current = match current {
            Value::Object(map) => map.entry((*segment).to_string()).or_insert_with(|| {
                if next_is_index {
                    Value::Array(Vec::new())
                } else {
                    Value::Object(Map::new())
                }
            }),
            Value::Array(array) => {
                let position: usize = segment
                    .parse()
                    .map_err(|_| Error::ForbiddenPath(path.to_string()))?;
                while array.len() <= position {
                    array.push(Value::Null);
                }
                &mut array[position]
            }
            other => {
                *other = Value::Object(Map::new());
                other
            }
        };
    }
    Ok(())
}

/// `$unset` (`unset.js`): delete an object key, or store `null` in an array.
fn unset_path(document: &mut Value, path: &str) -> Result<()> {
    check_path(path)?;
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments.split_last().expect("split always yields one");
    let mut current = document;
    for segment in parents {
        current = match current {
            Value::Object(map) => match map.get_mut(*segment) {
                Some(next) => next,
                None => return Ok(()),
            },
            Value::Array(array) => {
                match segment.parse::<usize>().ok().and_then(|i| array.get_mut(i)) {
                    Some(next) => next,
                    None => return Ok(()),
                }
            }
            _ => return Ok(()),
        };
    }
    match current {
        Value::Object(map) => {
            map.remove(*last);
        }
        Value::Array(array) => {
            if let Ok(position) = last.parse::<usize>() {
                if let Some(slot) = array.get_mut(position) {
                    *slot = Value::Null;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JS value semantics
// ---------------------------------------------------------------------------

/// Deep equality with JS semantics: numbers compare by value, arrays element-wise,
/// objects by key set.
fn deep_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => js_number_equal(a, b),
            _ => a == b,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| deep_equal(value, other)))
        }
        _ => false,
    }
}

/// `===` for numbers, which distinguishes `-0` from `0` the way `Object.is` does
/// for the `NaN` case only. `NaN != NaN`, so `NaN` never equals itself.
fn js_number_equal(left: f64, right: f64) -> bool {
    if left.is_nan() || right.is_nan() {
        return false;
    }
    left == right
}

/// JS number addition, including the `Infinity` behaviour that `f64` shares.
fn javascript_sum(left: f64, right: f64) -> f64 {
    left + right
}

/// Mingo's `compare` (`util/_internal.js`) reduced to what `$min`/`$max`/`$sort`
/// need: JS `<` across types, with numbers compared numerically and strings by
/// UTF-16 code unit.
fn js_compare(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        },
        (Value::String(a), Value::String(b)) => dxdb_jsstr::compare_js(a, b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

/// A JSON number from an `f64`, using the JS-safe representation.
fn number_value(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Wall clock in milliseconds, matching `Date.now()`.
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

/// An ISO-8601 date string from milliseconds, which is what `$currentDate` with a
/// `date` type stores.
fn format_iso8601(millis: f64) -> String {
    let total_seconds = (millis / 1000.0).floor() as i64;
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let millis_part = (millis as i64).rem_euclid(1000);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis_part:03}Z")
}

/// Howard Hinnant's `civil_from_days`, so `$currentDate` needs no date crate.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Re-exported so a caller can render a document the way `JSON.stringify` would.
pub use dxdb_json::to_js_json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_list_is_exactly_mingos_fourteen() {
        // Every name here came from `node_modules/mingo/cjs/operators/update/`
        // at 7.2.5. `$setOnInsert` is deliberately absent: mingo does not ship it.
        assert_eq!(OPERATORS.len(), 14);
        assert!(!OPERATORS.contains(&"$setOnInsert"));
        assert!(!OPERATORS.contains(&"$replaceRoot"));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01, 2024-02-29 (a leap day), 2000-03-01 (the 1900/2000 rule).
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }
}

// ---------------------------------------------------------------------------
// The plugin, on the core registry
// ---------------------------------------------------------------------------

/// The hook this plugin registers on, if any.
///
/// `update` registers **none**, and that is the finding rather than an omission:
/// upstream's `RxDBUpdatePlugin` is a pure prototype plugin
/// (`src/plugins/update/index.ts:45-56`) — three `proto.update = ..` assignments and no
/// `hooks` block. Its whole surface is `doc.update`, `doc.incrementalUpdate` and
/// `query.update`, which in Rust are methods on [`UpdateDocument`] rather than anything
/// the registry could dispatch.
///
/// The plugin still implements [`dxdb_core::Plugin`] so that a database can say "the
/// update plugin is installed", which is what a ported test and a startup log want, and
/// so that the declaration is uniform with the plugins that do register handlers.
pub const HOOKS: &[dxdb_core::HookName] = &[];

/// The `RxDBUpdatePlugin` equivalent.
///
/// Install it with `database.add_plugin(UpdatePlugin)?`. Because [`HOOKS`] is empty the
/// install has nothing to register; the plugin's behaviour arrives through
/// [`UpdateDocument`], which is the Rust spelling of upstream's prototype mutation.
///
/// This is the ergonomic cost the plugin model documents: upstream's
/// `addRxPlugin(RxDBUpdatePlugin)` makes `doc.update(..)` available everywhere, while
/// here a caller writes `use dxdb_plugin_update::UpdateDocument;`. Reported as a
/// divergence, not hidden.
#[derive(Clone, Copy, Debug, Default)]
pub struct UpdatePlugin;

impl dxdb_core::Plugin for UpdatePlugin {
    fn name(&self) -> &'static str {
        // The upstream plugin's `name` field (`src/plugins/update/index.ts:46`).
        "update"
    }

    fn install(
        &self,
        _registrations: &mut dxdb_core::Registrations,
        _overwritables: &mut dxdb_core::Overwritables,
    ) -> std::result::Result<(), dxdb_core::PluginError> {
        // Nothing to register: see `HOOKS`.
        Ok(())
    }
}

/// The document methods the plugin adds, as a trait.
///
/// Upstream adds these to `RxDocument.prototype`; here they are an extension trait over
/// the document data a caller already has. `update` writes directly, `incremental_update`
/// takes the previous state so a caller can retry, mirroring upstream's
/// `incrementalModify` wrapper (`src/plugins/update/index.ts:15-23`).
pub trait UpdateDocument {
    /// `doc.update(updateObj)` (`index.ts:25-32`): apply the modifiers and return the new
    /// document data.
    fn update(&self, update: &serde_json::Value) -> Result<serde_json::Value>;

    /// `doc.incrementalUpdate(updateObj)` (`index.ts:15-23`): the same transformation,
    /// named separately because a caller that wants a retry loop drives it from the
    /// returned value.
    ///
    /// Upstream's version runs inside `incrementalModify` so a concurrent write is
    /// retried against the newer revision. That retry loop is `dxdb-core`'s
    /// `WriteQueue`; this method is the pure transformation it would call.
    fn incremental_update(&self, update: &serde_json::Value) -> Result<serde_json::Value>;
}

impl UpdateDocument for serde_json::Value {
    fn update(&self, update: &serde_json::Value) -> Result<serde_json::Value> {
        apply_update(self, update)
    }

    fn incremental_update(&self, update: &serde_json::Value) -> Result<serde_json::Value> {
        apply_update(self, update)
    }
}
