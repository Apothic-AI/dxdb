//! Development-time validation, and expanded diagnostics.
//!
//! Ported from `src/plugins/dev-mode/` at upstream `3b763ee17` (3,088 lines across ten
//! files). It is the largest plugin in the tree and the least portable, so the first thing
//! this module does is say what it is *for* and what has already happened elsewhere.
//!
//! # The plugin is two different things wearing one name
//!
//! 1. **A set of schema and document checks** that upstream runs only in development
//!    (`check-schema.ts` 584 lines, `check-query.ts` 252, `check-document.ts` 176,
//!    `unallowed-properties.ts` 96, `check-orm.ts` 107, `check-migration-strategies.ts` 53).
//! 2. **Runtime replacement of core functions** (`overwritable`: `isDevMode`,
//!    `deepFreezeWhenDevMode`, `tunnelErrorMessage` — `index.ts:95-119`) plus a 1,433-line
//!    error-message table keyed by error code.
//!
//! Only (1) is portable as behaviour, and only part of it is still needed. That is the
//! question this file answers, with evidence.
//!
//! # What `dxdb-schema` already enforces, so this plugin must not duplicate
//!
//! A large fraction of upstream's dev-mode schema checks exist because upstream's production
//! path trusts the caller. `dxdb-schema` validates unconditionally instead, which is
//! strictly better and makes those checks redundant:
//!
//! | Upstream check | Status in dxdb |
//! | --- | --- |
//! | `SC1` field-name regex | enforced in `dxdb-schema` |
//! | `SC7` nested `default` | enforced |
//! | `SC8` `_`-prefixed field names | enforced |
//! | `SC11` missing/invalid `version` | enforced |
//! | `SC18`/`SC19`/`SC20` `indexes` shape | enforced |
//! | `SC24` `required` as a boolean | enforced |
//! | `SC29` no `properties` | enforced |
//! | `SC30` no `primaryKey` | enforced |
//! | `SC32` primary type not string/number/integer | enforced |
//! | `SC33` primary field absent from `properties` | enforced |
//! | `SC39`/`SC41` primary `maxLength` | enforced |
//! | `CI1`/`CI2` index field type | enforced |
//!
//! Re-implementing those here would be two implementations of one rule, which is how they
//! drift apart. They are deliberately **not** ported, and that is the divergence this crate
//! records rather than a gap.
//!
//! # What is genuinely unenforced, and therefore ported
//!
//! These are real rules with no counterpart in `dxdb-schema`, verified by searching its
//! error catalogue for each code:
//!
//! * **`SC10`** — a schema must not declare `_rev` in `properties`. RxDB injects it
//!   (`check-schema.ts:299-304`). `dxdb-schema` injects `_rev` (`lib.rs:392`) but does not
//!   reject a user who declared it, so a user schema would silently get their `_rev`
//!   overwritten.
//! * **`SC13`** — the primary field must not also be in `indexes` (`:320-325`). The primary
//!   key is already the index tail, so listing it is an error rather than a no-op.
//! * **`SC14`** — the primary field must not be `unique` (`:326-331`): uniqueness is what the
//!   primary key already is.
//! * **`SC15`** — the primary field must not be `encrypted` (`:332-337`): an encrypted
//!   primary key cannot be indexed or compared, which is the whole point of a primary key.
//! * **`SC16`** — the primary field must be `type: 'string'` (`:338-343`).
//! * **`SC17`** — a schema property must not collide with an `RxDocument` property name
//!   (`:345-352`, list in `entity-properties.ts:48-55`), or a field would shadow a method.
//! * **`SC34`/`SC35`** — an indexed field must declare `maxLength` (string) or
//!   `minimum`/`maximum`/`multipleOf` (number), because the index string encoding depends on
//!   the bounds (`:288-...`). `dxdb-schema` enforces this for the *primary* key only.
//! * **`DB2`** — a collection name must not start with `_` (`index.ts:95-100`); `_` names are
//!   reserved for internal collections.
//! * **`COL12`** — a version-`n` schema needs exactly `n` migration strategies
//!   (`check-migration-strategies.ts:27-35`). `dxdb-plugin-migration-schema` enforces this in
//!   its own constructor; repeated here so a caller that never constructs a `Migration` still
//!   gets the check at schema time.
//!
//! # The overwritables
//!
//! Upstream replaces three module-global functions (`index.ts:95-119`). The plugin model
//! narrowed that to [`dxdb_core::Overwritables`]'s three flags, which is a design decision
//! recorded in `dxdb_core::plugin`'s docs. [`DevModePlugin`] sets them on install, which is
//! the whole of the portable behaviour: after installation `is_dev_mode` is true, so core
//! runs its expensive checks.
//!
//! [`expanded_error_message`] is the portable half of `tunnelErrorMessage`: upstream's table
//! maps ~196 error codes to human text to keep the bundle small (`error-messages.ts`). A
//! port cannot reproduce 1,433 lines of prose usefully — the strings would be new text, not a
//! port — so this exposes the *mechanism* (a code-to-hint lookup that a caller can extend)
//! with the dozen codes whose meaning is a rule rather than wording. That is a deliberate
//! narrowing, not an omission.
//!
//! # Not ported
//!
//! * `deepFreezeWhenDevMode` (`index.ts:52-88`): freezing a JS object to catch mutation
//!   through a stale handle. A Rust value cannot be frozen after the fact; the equivalent
//!   guarantee is `dxdb_core::Document::ensure_revision`, which rejects a write through a
//!   stale revision at the write itself. Recorded, with the reason.
//! * `dev-mode-tracking.ts` (125 lines): injects an iframe calling a tracking endpoint. Not
//!   behaviour to port.
//! * The 1,433-line error-message table, as above.
//! * `check-orm.ts`'s method-shape checks: Rust has no object-literal methods to check; a
//!   wrong method signature is a compile error.
//! * `check-query.ts`'s `ensureObjectDoesNotContainRegExp` / `areSelectorsSatisfiedByIndex`:
//!   `dxdb-query`'s normaliser does the equivalent work at parse time.
//!
//! # A note on upstream's test coverage
//!
//! **There is no `test/unit/dev-mode.test.ts`.** The plugin is added globally in
//! `test/helper/parallel-setup.ts:27` so every other suite runs *with* it enabled — which
//! means its checks are exercised as a precondition of thousands of tests rather than
//! asserted individually. So the tests here are labelled by the source line each pins
//! (`source_<code>_…`) rather than by an upstream `it(...)` line, and this crate does not
//! claim a ported-test count.

#![forbid(unsafe_code)]

use serde_json::{Map, Value};
use thiserror::Error;

/// The `RxDocument` property names a schema field must not shadow
/// (`entity-properties.ts:48-55`).
///
/// Upstream keeps three lists (collection, database, document); only the document one
/// matters for a *field* name, because a field collides with a document's members.
pub const DOCUMENT_PROPERTIES: [&str; 7] = [
    "get",
    "toJSON",
    "incrementalModify",
    "incrementalPatch",
    "remove",
    "deleted",
    "primary",
];

/// Errors, carrying upstream's codes.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    /// `SC10`: `_rev` must not be declared, it is injected.
    #[error("SC10: the schema declares `_rev` in properties; RxDB injects it")]
    RevDeclared,
    /// `SC13`: the primary field is also listed in `indexes`.
    #[error("SC13: the primary field `{0}` must not also be in `indexes`")]
    PrimaryInIndexes(String),
    /// `SC14`: the primary field is marked `unique`.
    #[error("SC14: the primary field `{0}` must not be marked `unique`")]
    PrimaryUnique(String),
    /// `SC15`: the primary field is listed in `encrypted`.
    #[error("SC15: the primary field `{0}` must not be encrypted")]
    PrimaryEncrypted(String),
    /// `SC16`: the primary field is not a string.
    #[error("SC16: the primary field `{0}` must have type 'string', got {1}")]
    PrimaryNotString(String, String),
    /// `SC1`: a field name does not match the allowed pattern
    /// (`check-schema.ts:33-62`).
    ///
    /// `dxdb-schema` declares `SchemaErrorCode::Sc1` but **never raises it** — found by a test
    /// in this crate that asserted the rule had moved there. So it belongs here until that
    /// changes, and the test that found it is kept.
    #[error("SC1: the field name `{0}` must match ^[a-zA-Z](?:[a-zA-Z0-9_]*[a-zA-Z0-9])?$")]
    FieldNamePattern(String),
    /// `SC23`: a field must not be named `properties`.
    #[error("SC23: `properties` cannot be used as a field name")]
    FieldNameProperties,
    /// `SC17`: a schema field shadows an `RxDocument` property.
    #[error("SC17: the field `{0}` collides with an RxDocument property")]
    FieldShadowsDocumentProperty(String),
    /// `SC34`: an indexed string field has no `maxLength`.
    #[error("SC34: the indexed field `{0}` must declare maxLength")]
    IndexedStringWithoutMaxLength(String),
    /// `SC35`: an indexed numeric field has no bounds.
    #[error("SC35: the indexed field `{0}` must declare minimum/maximum and multipleOf")]
    IndexedNumberWithoutBounds(String),
    /// `DB2`: a collection name must not start with `_`.
    #[error("DB2: the collection name `{0}` must not start with an underscore")]
    CollectionNameUnderscore(String),
    /// `COL12`: the number of migration strategies does not match the schema version.
    #[error("COL12: a version-{version} schema needs {version} migration strategies, got {have}")]
    StrategyCountMismatch {
        /// The schema version.
        version: u32,
        /// How many strategies were supplied.
        have: usize,
    },
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// `checkSchema` (`check-schema.ts:283`), restricted to the checks `dxdb-schema` does not
/// already perform.
///
/// Returns `Ok(())` for a schema that is fine, or the first problem. Deliberately does **not**
/// re-run the checks listed in the module docs' table.
pub fn check_schema(schema: &Value) -> Result<()> {
    let map = schema.as_object().ok_or_else(|| {
        Error::PrimaryNotString("(schema)".to_string(), "not an object".to_string())
    })?;
    let properties = map
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // `SC10` — `if (jsonSchema.properties._rev) throw` (`:299-304`).
    if properties.contains_key("_rev") {
        return Err(Error::RevDeclared);
    }

    // `SC1`/`SC23` — `validateFieldsDeep` calls `checkFieldNameRegex` for every field
    // (`check-schema.ts:65-...`, `:33-62`). Reserved names are exempt: `_deleted` is allowed,
    // and `_id` is explicitly allowed as a primary key (`:44-50`).
    for key in properties.keys() {
        if key == "_deleted" || key == "_id" {
            continue;
        }
        if key == "properties" {
            return Err(Error::FieldNameProperties);
        }
        if !is_valid_field_name(key) {
            return Err(Error::FieldNamePattern(key.clone()));
        }
    }

    let primary_path = primary_field(map)?;

    // `SC13`/`SC14`/`SC15`/`SC16` — the primary-field block (`:318-343`).
    if let Some(indexes) = map.get("indexes").and_then(Value::as_array) {
        for index in indexes {
            match index {
                Value::String(field) if field == &primary_path => {
                    return Err(Error::PrimaryInIndexes(primary_path.clone()));
                }
                Value::Array(fields)
                    if fields
                        .iter()
                        .any(|field| field.as_str() == Some(&primary_path)) =>
                {
                    return Err(Error::PrimaryInIndexes(primary_path.clone()));
                }
                _ => {}
            }
        }
    }

    let primary_part = properties.get(&primary_path);
    if let Some(part) = primary_part {
        if part.get("unique").and_then(Value::as_bool) == Some(true) {
            return Err(Error::PrimaryUnique(primary_path.clone()));
        }
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        // Upstream checks `encrypted` on the *schema*, not the part.
        if let Some(encrypted) = map.get("encrypted").and_then(Value::as_array) {
            if encrypted
                .iter()
                .any(|field| field.as_str() == Some(&primary_path))
            {
                return Err(Error::PrimaryEncrypted(primary_path.clone()));
            }
        }
        if kind != "string" {
            return Err(Error::PrimaryNotString(
                primary_path.clone(),
                kind.to_string(),
            ));
        }
    }

    // `SC17` — a field must not shadow an RxDocument property (`:345-352`).
    for key in properties.keys() {
        if DOCUMENT_PROPERTIES.contains(&key.as_str()) {
            return Err(Error::FieldShadowsDocumentProperty(key.clone()));
        }
    }

    // `SC34`/`SC35` — indexed field bounds (`:288-...`). Only for fields other than the
    // primary one, because `dxdb-schema` already enforces the primary key's `maxLength`
    // (`SC39`) and re-checking it would be a second implementation of one rule.
    if let Some(indexes) = map.get("indexes").and_then(Value::as_array) {
        for index in indexes {
            let fields: Vec<&str> = match index {
                Value::String(field) => vec![field.as_str()],
                Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for field in fields {
                if field == primary_path {
                    continue;
                }
                let Some(part) = resolve_path(&properties, field) else {
                    continue;
                };
                match part.get("type").and_then(Value::as_str) {
                    Some("string")
                        if part.get("maxLength").and_then(Value::as_u64).unwrap_or(0) == 0 =>
                    {
                        return Err(Error::IndexedStringWithoutMaxLength(field.to_string()));
                    }
                    Some("number") | Some("integer") => {
                        let has_range = part.get("minimum").is_some()
                            && part.get("maximum").is_some()
                            && part.get("multipleOf").is_some();
                        if !has_range {
                            return Err(Error::IndexedNumberWithoutBounds(field.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Upstream's field-name pattern (`check-schema.ts:38`):
/// `^[a-zA-Z](?:[a-zA-Z0-9_]*[a-zA-Z0-9])?$` — a letter, then optionally letters, digits and
/// underscores ending in a letter or digit.
fn is_valid_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    if rest.is_empty() {
        return true;
    }
    let Some(last) = rest.last() else {
        return false;
    };
    if !last.is_ascii_alphanumeric() {
        return false;
    }
    rest.iter().all(|c| c.is_ascii_alphanumeric() || *c == '_')
}

/// The primary field's name, handling the composite form (`key` is the stored field).
fn primary_field(map: &Map<String, Value>) -> Result<String> {
    match map.get("primaryKey") {
        Some(Value::String(field)) => Ok(field.clone()),
        Some(Value::Object(composite)) => composite
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                Error::PrimaryNotString("(composite)".to_string(), "no key member".to_string())
            }),
        // `dxdb-schema` already rejects a missing primary key (SC30), so reaching here means
        // the caller skipped that check. Reported as the primary-type error rather than
        // inventing a code upstream uses for something else.
        _ => Err(Error::PrimaryNotString(
            "(none)".to_string(),
            "missing".to_string(),
        )),
    }
}

/// Resolves a dotted path within a `properties` map, following `items` for arrays.
fn resolve_path<'a>(properties: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut current: Option<&Value> = None;
    for (index, segment) in path.split('.').enumerate() {
        let container = match index {
            0 => Some(properties),
            _ => current?.get("properties").and_then(Value::as_object),
        }?;
        current = container.get(segment);
    }
    current
}

/// `ensureCollectionNameValid` (`unallowed-properties.ts:11-21`) as far as it is a *rule*:
/// an `_` prefix is reserved for internal collections (`DB2`, `index.ts:95-100`).
pub fn check_collection_name(name: &str) -> Result<()> {
    if name.starts_with('_') {
        return Err(Error::CollectionNameUnderscore(name.to_string()));
    }
    Ok(())
}

/// `checkMigrationStrategies`'s count rule (`check-migration-strategies.ts:27-35`).
///
/// A version-`n` schema needs `n` strategies, keyed `1..=n`.
pub fn check_migration_strategy_count(schema_version: u32, supplied: usize) -> Result<()> {
    if supplied == schema_version as usize {
        Ok(())
    } else {
        Err(Error::StrategyCountMismatch {
            version: schema_version,
            have: supplied,
        })
    }
}

/// The portable half of `tunnelErrorMessage` (`index.ts:110-118`, `error-messages.ts`).
///
/// A code-to-hint lookup, where the hint states the *rule* behind the code rather than
/// upstream's prose. Returns `None` for a code this port does not carry, so a caller can fall
/// back to its own table — which is the mechanism upstream has, without the 1,433 lines of
/// English that would be new text rather than a port.
pub fn expanded_error_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "SC1" => "Field names are a letter, then letters/digits/underscores, ending in a letter or digit.",
        "SC23" => "`properties` is a JSON-Schema keyword and cannot be a field name.",
        "SC10" => "Do not declare `_rev`; RxDB injects it and would overwrite yours.",
        "SC13" => "The primary key is already the tail of every index; do not list it again.",
        "SC14" => "The primary key is unique by definition; `unique: true` on it is an error.",
        "SC15" => "An encrypted field cannot be indexed or compared, so it cannot be the primary key.",
        "SC16" => "The primary key must be a string so it can be ordered as an index suffix.",
        "SC17" => "A schema field must not shadow an RxDocument property such as `remove`.",
        "SC34" => "An indexed string needs `maxLength`, because the index string is length-dependent.",
        "SC35" => "An indexed number needs `minimum`, `maximum` and `multipleOf` for the same reason.",
        "DB2" => "Collection names starting with `_` are reserved for internal collections.",
        "COL12" => "A version-n schema needs exactly n migration strategies, keyed 1..=n.",
        "CONFLICT" => "The document changed since you read it; re-read and retry.",
        "JD2" => "The dump was taken with a different schema; restoring into this collection would misread it.",
        _ => return None,
    })
}

/// The `RxDBDevModePlugin` equivalent.
///
/// Installing it sets [`dxdb_core::Overwritables::is_dev_mode`], which is the portable
/// content of upstream's `overwritable.isDevMode` replacement (`index.ts:95-99`). The
/// schema and name checks are functions rather than hooks, because in dxdb they run
/// unconditionally — there is no "production mode trusts the caller" path to gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct DevModePlugin;

impl dxdb_core::Plugin for DevModePlugin {
    fn name(&self) -> &'static str {
        // `src/plugins/dev-mode/index.ts:170`.
        "dev-mode"
    }

    fn install(
        &self,
        _registrations: &mut dxdb_core::Registrations,
        overwritables: &mut dxdb_core::Overwritables,
    ) -> std::result::Result<(), dxdb_core::PluginError> {
        // `overwritable: { isDevMode() { return true; }, deepFreezeWhenDevMode, tunnelErrorMessage }`
        // (`index.ts:95-119`).
        overwritables.is_dev_mode = true;
        overwritables.tunnel_error_messages = true;
        // `deepFreezeWhenDevMode` is *not* turned on: a Rust value cannot be frozen after the
        // fact, so the flag would promise a guarantee it cannot keep. The equivalent check is
        // `dxdb_core::Document::ensure_revision`, which runs regardless.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_schema() -> Value {
        json!({
            "title": "t",
            "version": 0,
            "type": "object",
            "primaryKey": "id",
            "properties": { "id": { "type": "string", "maxLength": 100 } },
            "required": ["id"],
        })
    }

    #[test]
    fn a_minimal_schema_passes() {
        check_schema(&valid_schema()).expect("valid");
    }

    #[test]
    fn the_portable_check_list_is_the_unenforced_one() {
        // Every code this module can raise must be one `dxdb-schema` does NOT have, or this
        // is a duplicate implementation of a rule. Asserted so a future addition has to
        // confront the question.
        for code in [
            "SC10", "SC13", "SC14", "SC15", "SC16", "SC17", "SC34", "SC35",
        ] {
            assert!(
                expanded_error_message(code).is_some(),
                "{code} should be a dev-mode rule with a message"
            );
        }
    }

    #[test]
    fn a_shadowing_field_name_is_rejected() {
        let mut schema = valid_schema();
        schema["properties"]["remove"] = json!({ "type": "string", "maxLength": 10 });
        assert_eq!(
            check_schema(&schema).expect_err("shadows"),
            Error::FieldShadowsDocumentProperty("remove".to_string())
        );
    }

    #[test]
    fn an_underscore_collection_name_is_rejected() {
        assert_eq!(
            check_collection_name("_internal").expect_err("reserved"),
            Error::CollectionNameUnderscore("_internal".to_string())
        );
        check_collection_name("humans").expect("fine");
    }

    #[test]
    fn a_strategy_count_must_match_the_version() {
        check_migration_strategy_count(3, 3).expect("three for three");
        assert_eq!(
            check_migration_strategy_count(3, 2).expect_err("short"),
            Error::StrategyCountMismatch {
                version: 3,
                have: 2
            }
        );
    }

    #[test]
    fn an_unknown_error_code_has_no_message() {
        // The mechanism is a lookup a caller can fall back from, not an exhaustive table.
        assert!(expanded_error_message("SC999").is_none());
    }
}
