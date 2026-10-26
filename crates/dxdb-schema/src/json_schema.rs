//! JSON Schema validation, draft-07.
//!
//! RxDB validates a schema against itself when a collection is created, and
//! validates every document against the schema on insert and update. Both go
//! through draft-07 (`PLAN.md` section 9: "draft-07 (that is RxDB's draft)").
//!
//! The validation crate is `jsonschema` 0.60.1. Two configuration choices are
//! deliberate:
//!
//! - **Format checking is off.** RxDB does not check `format` unless the
//!   `validate-formats` style plugin is added; a schema with `format:
//!   "date-time"` aboard must accept the same documents it accepts upstream.
//! - **Unknown formats are ignored** rather than erroring, for the same reason:
//!   `format` is documentation in the default stack.
//!
//! The validator is built once per schema. Upstream does the same — a
//! `Validator` owns a compiled keyword tree, and rebuilding it per document would
//! dominate insert cost.

use jsonschema::{Draft, Validator};
use serde_json::Value;

use crate::error::{SchemaError, SchemaErrorCode};

/// A compiled draft-07 validator for one schema.
///
/// `jsonschema::Validator` is `Send + Sync` (PLAN.md section 9 records the probe),
/// so this can be shared across tasks; it is not `Clone`, which is why callers
/// hold it behind an `Arc` rather than copying it.
pub struct SchemaValidator {
    inner: Validator,
}

impl std::fmt::Debug for SchemaValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaValidator").finish_non_exhaustive()
    }
}

impl SchemaValidator {
    /// Compiles a schema. Fails if the schema is not a valid draft-07 schema at
    /// all, which is a programming error in the schema rather than a document
    /// problem.
    pub fn new(schema: &Value) -> Result<Self, SchemaError> {
        let inner = jsonschema::options()
            .with_draft(Draft::Draft7)
            // Upstream's default stack does not validate `format`; the
            // validate-ajv / validate-is-my-json-valid plugins do, and they are
            // M6 work.
            .should_validate_formats(false)
            .should_ignore_unknown_formats(true)
            .build(schema)
            .map_err(|error| {
                SchemaError::new(
                    SchemaErrorCode::InvalidSchema,
                    format!("schema is not valid draft-07: {error}"),
                )
            })?;
        Ok(Self { inner })
    }

    /// Returns every validation error, in the order the validator produced them,
    /// as `path: message` strings.
    ///
    /// Returning all errors rather than the first matches what a developer
    /// debugging a rejected document needs, and it is what upstream's validation
    /// error aggregates carry.
    pub fn errors(&self, document: &Value) -> Vec<String> {
        self.inner
            .iter_errors(document)
            .map(|error| {
                let path = error.instance_path().to_string();
                if path.is_empty() {
                    error.to_string()
                } else {
                    format!("{path}: {error}")
                }
            })
            .collect()
    }
}

/// Validates a document against a schema, returning every error.
///
/// An empty result means the document is valid.
pub fn validate_document(schema: &Value, document: &Value) -> Result<Vec<String>, SchemaError> {
    Ok(SchemaValidator::new(schema)?.errors(document))
}

/// Validates a schema against the draft-07 meta-schema.
///
/// This is the check upstream performs when a schema is created: a schema whose
/// own shape is invalid (an `indexes` that is not an array of strings, a
/// `properties` that is not an object) must fail before it reaches a storage.
pub fn validate_json_schema(schema: &Value) -> Result<(), SchemaError> {
    let Some(object) = schema.as_object() else {
        return Err(SchemaError::new(
            SchemaErrorCode::InvalidSchema,
            "schema must be a JSON object",
        ));
    };
    if !object.contains_key("primaryKey") {
        return Err(SchemaError::new(
            SchemaErrorCode::Sc30,
            "schema has no primaryKey",
        ));
    }
    match object.get("properties") {
        Some(Value::Object(_)) => {}
        _ => {
            return Err(SchemaError::new(
                SchemaErrorCode::Sc29,
                "schema has no `properties` object",
            ))
        }
    }
    validate_draft7(schema)
}

/// Validates a schema against the draft-07 meta-schema only.
///
/// Explicitly the draft-07 module rather than `jsonschema::meta::validate`: the
/// latter detects the draft from `$schema`, and a RxDB schema usually has no
/// `$schema` at all, in which case detection falls back to the newest bundled
/// draft and would reject draft-07-only constructs. RxDB is draft-07 and this
/// says so.
fn validate_draft7(schema: &Value) -> Result<(), SchemaError> {
    match jsonschema::draft7::meta::validate(schema) {
        Ok(()) => Ok(()),
        Err(error) => Err(SchemaError::new(
            SchemaErrorCode::InvalidSchema,
            format!("schema is not valid draft-07: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn draft7_accepts_and_rejects() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "minLength": 1},
                "age": {"type": "integer", "minimum": 0}
            },
            "required": ["id"],
            "additionalProperties": false
        });
        let validator = SchemaValidator::new(&schema).expect("schema compiles");
        assert!(validator.errors(&json!({"id": "a", "age": 3})).is_empty());
        assert!(!validator.errors(&json!({"age": 3})).is_empty());
        assert!(!validator.errors(&json!({"id": "a", "age": -1})).is_empty());
        assert!(!validator.errors(&json!({"id": "a", "extra": 1})).is_empty());
    }

    #[test]
    fn format_is_not_validated_by_default() {
        // Upstream's default stack treats `format` as documentation.
        let schema = json!({
            "type": "object",
            "properties": { "when": {"type": "string", "format": "date-time"} }
        });
        let validator = SchemaValidator::new(&schema).expect("schema compiles");
        assert!(validator.errors(&json!({"when": "not a date"})).is_empty());
    }

    #[test]
    fn draft7_keywords_that_draft4_lacks_are_honoured() {
        // `if`/`then` and `const` are draft-07 additions; a draft-04 validator
        // would silently ignore them.
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {"const": "person"},
                "name": {"type": "string"}
            },
            "if": { "properties": { "kind": { "const": "person" } } },
            "then": { "required": ["name"] }
        });
        let validator = SchemaValidator::new(&schema).expect("schema compiles");
        assert!(validator
            .errors(&json!({"kind": "person", "name": "a"}))
            .is_empty());
        assert!(!validator.errors(&json!({"kind": "person"})).is_empty());
        assert!(!validator.errors(&json!({"kind": "robot"})).is_empty());
    }
}
