//! Document fixtures, ported from upstream `src/plugins/test-utils/schema-objects.ts`.
//!
//! Upstream's generators take optional arguments and fill the rest randomly:
//! `humanData(passportId = randomStringWithSpecialChars(8,12), age =
//! randomNumber(10,50), firstName = randomStringWithSpecialChars(8,12))`. The
//! random defaults are kept here, because a case that calls `human_data()` with no
//! arguments is testing "some document", and pinning it would silently narrow the
//! input space the suite covers.
//!
//! # Divergence, and it is deliberate
//!
//! Upstream also randomises `lastName`, and so does [`human_data`]. But the
//! query-correctness suite passes the other three arguments explicitly and leaves
//! `lastName` random, and **no query in either suite filters or sorts on
//! `lastName`** (verified: `grep lastName` over the correctness suite is empty).
//! A random value there would make the generated fixture data irreproducible for
//! no testing benefit, so the generated data commits `last-<passportId>` and says
//! so. A future case that depends on `lastName` must pass it explicitly, which is
//! enforced by there being no `human_data` overload that omits it from a
//! multi-argument call.
//!
//! # Why `serde_json::Value` and not a struct
//!
//! A storage never sees a typed document (PLAN.md 3.1: documents are ordered JSON),
//! so a typed fixture would be converted to JSON before it was stored. Mirroring
//! upstream's plain object literal keeps the port one-to-one and makes an extra or
//! missing field visible in the diff.

use serde_json::{json, Map, Value};

use crate::util::{random_number, random_string_with_special_chars, random_token};

/// `humanData(passportId, age, firstName)` — `schema-objects.ts:73`.
///
/// The four-field `HumanDocumentType` of the `human` schema. Arguments that are
/// given are used verbatim; `last_name` is drawn from the same special-character
/// charset upstream uses, because a storage that mangles an umlaut in *any* field
/// is broken.
pub fn human_data(passport_id: &str, age: i64, first_name: &str) -> Value {
    json!({
        "passportId": passport_id,
        "firstName": first_name,
        "lastName": random_string_with_special_chars(8, 12),
        "age": age,
    })
}

/// `humanData()` with every argument drawn randomly — `schema-objects.ts:73`.
pub fn random_human_data() -> Value {
    human_data(
        &random_string_with_special_chars(8, 12),
        random_number(10, 50),
        &random_string_with_special_chars(8, 12),
    )
}

/// `simpleHumanData()` — `schema-objects.ts:86`. No `age`; used for
/// `simpleHumanAge`, whose schema has `age` as a *string*.
pub fn simple_human_data() -> Value {
    json!({
        "passportId": random_string_with_special_chars(8, 12),
        "firstName": random_string_with_special_chars(8, 12),
        "lastName": random_string_with_special_chars(8, 12),
    })
}

/// `simpleHumanV3Data(partial)` — `schema-objects.ts:99`.
///
/// Upstream merges the partial over the random defaults with `Object.assign`, i.e.
/// a key in `partial` wins and a key absent from it keeps the default. `null`
/// therefore sets a key to null rather than removing it, which is exactly the
/// distinction the correctness suite's "Using undefined in a selector" group
/// depends on.
pub fn simple_human_v3_data(partial: Map<String, Value>) -> Value {
    let mut document = Map::new();
    document.insert(
        "passportId".to_string(),
        Value::String(random_string_with_special_chars(8, 12)),
    );
    document.insert("age".to_string(), json!(random_number(10, 50)));
    for (key, value) in partial {
        document.insert(key, value);
    }
    Value::Object(document)
}

/// `nestedHumanData(partial)` — `schema-objects.ts:159`.
///
/// `mainSkill.level` defaults to the fixed `5` upstream uses, and `name` to a
/// 4-6 character special-character string.
pub fn nested_human_data(partial: Map<String, Value>) -> Value {
    let mut document = Map::new();
    document.insert(
        "passportId".to_string(),
        Value::String(random_string_with_special_chars(8, 12)),
    );
    document.insert(
        "firstName".to_string(),
        Value::String(random_string_with_special_chars(8, 12)),
    );
    document.insert(
        "mainSkill".to_string(),
        json!({
            "name": random_string_with_special_chars(4, 6),
            "level": 5,
        }),
    );
    for (key, value) in partial {
        document.insert(key, value);
    }
    Value::Object(document)
}

/// `deepNestedHumanData()` — `schema-objects.ts:184`.
pub fn deep_nested_human_data() -> Value {
    json!({
        "passportId": random_string_with_special_chars(8, 12),
        "mainSkill": {
            "name": random_string_with_special_chars(4, 6),
            "attack": { "good": false, "count": 5 }
        }
    })
}

/// `bigHumanDocumentType()` — `schema-objects.ts:204`.
pub fn big_human_data() -> Value {
    json!({
        "passportId": random_string_with_special_chars(8, 12),
        "dnaHash": random_string_with_special_chars(8, 12),
        "firstName": random_string_with_special_chars(8, 12),
        "lastName": random_string_with_special_chars(8, 12),
        "age": random_number(10, 50),
    })
}

/// `heroArrayData()` — `schema-objects.ts:221`. Three skills per hero.
pub fn hero_array_data() -> Value {
    let skills: Vec<Value> = (0..3)
        .map(|_| {
            json!({
                "name": random_string_with_special_chars(4, 6),
                "damage": random_number(10, 50),
            })
        })
        .collect();
    json!({
        "name": random_string_with_special_chars(6, 8),
        "skills": skills,
    })
}

/// `simpleHeroArray(partial)` — `schema-objects.ts:237`. Three string skills.
pub fn simple_hero_array(partial: Map<String, Value>) -> Value {
    let mut document = Map::new();
    document.insert(
        "name".to_string(),
        Value::String(random_string_with_special_chars(8, 10)),
    );
    let skills: Vec<Value> = (0..3)
        .map(|_| Value::String(random_string_with_special_chars(3, 6)))
        .collect();
    document.insert("skills".to_string(), Value::Array(skills));
    for (key, value) in partial {
        document.insert(key, value);
    }
    Value::Object(document)
}

/// `averageSchemaData(partial)` — `schema-objects.ts:432`.
///
/// Field lengths are derived from the *schema's own* `maxLength` values upstream
/// (it reads `averageSchemaForFieldLength.properties.id.maxLength` and friends), so
/// the values sit at the boundary the index encoding cares about. The same
/// derivation is done here from the numbers in [`crate::schemas::average_schema`],
/// and [`crate::schemas::average_schema`] carries the same `maxLength`s.
///
/// Note `deep.deeper.deepNr` and the five `list` entries: upstream adds fields the
/// schema does not declare. A storage that validated strictly would reject them;
/// upstream's does not, and the key-compression case depends on the extra depth.
pub fn average_schema_data(partial: Map<String, Value>) -> Value {
    let mut document = Map::new();
    // id.maxLength = 12 -> 11..=12
    document.insert(
        "id".to_string(),
        Value::String(random_string_with_special_chars(11, 12)),
    );
    // var1.maxLength = 12 -> 9..=12
    document.insert(
        "var1".to_string(),
        Value::String(random_string_with_special_chars(9, 12)),
    );
    // var2.maximum = 50000, and upstream draws 100..=maximum
    document.insert("var2".to_string(), json!(random_number(100, 50000)));
    // deep.deep1/.deep2 maxLength = 10 -> 7..=10
    document.insert(
        "deep".to_string(),
        json!({
            "deep1": random_string_with_special_chars(7, 10),
            "deep2": random_string_with_special_chars(7, 10),
            "deeper": { "deepNr": random_number(0, 10) },
        }),
    );
    let list: Vec<Value> = (0..5)
        .map(|_| {
            json!({
                "deep1": random_string_with_special_chars(2, 5),
                "deep2": random_string_with_special_chars(5, 8),
            })
        })
        .collect();
    document.insert("list".to_string(), Value::Array(list));
    for (key, value) in partial {
        document.insert(key, value);
    }
    Value::Object(document)
}

/// `pointData()` — `schema-objects.ts:462`.
pub fn point_data() -> Value {
    json!({
        "id": random_string_with_special_chars(8, 12),
        "x": random_number(1, 100),
        "y": random_number(1, 100),
    })
}

/// `humanWithSubOther()` — `schema-objects.ts:131`.
pub fn human_with_sub_other() -> Value {
    json!({
        "passportId": random_string_with_special_chars(8, 12),
        "other": { "age": random_number(10, 50) },
    })
}

/// `NoIndexHuman()` — `schema-objects.ts:144`.
pub fn no_index_human() -> Value {
    json!({
        "firstName": random_string_with_special_chars(8, 12),
        "lastName": random_string_with_special_chars(8, 12),
    })
}

/// A document for the implementations suite's `TestDocType`
/// (`rx-storage-implementations.test.ts:76` `getWriteData`).
///
/// Returns the document *without* the meta fields, because the meta fields'
/// values are what each case varies. [`crate::fixtures::write_data`] adds them.
pub fn test_doc(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": value })
}

/// `EXAMPLE_REVISION_1` — `test-utils/revisions.ts:1`.
pub const EXAMPLE_REVISION_1: &str = "1-12080c42d471e3d2625e49dcca3b8e1a";
/// `EXAMPLE_REVISION_2` — `test-utils/revisions.ts:2`.
pub const EXAMPLE_REVISION_2: &str = "2-22080c42d471e3d2625e49dcca3b8e2b";
/// `EXAMPLE_REVISION_3` — `test-utils/revisions.ts:3`.
pub const EXAMPLE_REVISION_3: &str = "3-32080c42d471e3d2625e49dcca3b8e3c";
/// `EXAMPLE_REVISION_4` — `test-utils/revisions.ts:4`.
pub const EXAMPLE_REVISION_4: &str = "4-42080c42d471e3d2625e49dcca3b8e3c";

/// Every revision constant, in order.
pub const EXAMPLE_REVISIONS: [&str; 4] = [
    EXAMPLE_REVISION_1,
    EXAMPLE_REVISION_2,
    EXAMPLE_REVISION_3,
    EXAMPLE_REVISION_4,
];

/// The largest single UTF-16 code unit in [`TEST_DATA_CHARSET`], re-exported so a
/// case that asserts an index ordering does not have to recompute it.
pub fn test_data_charset_last_sorted() -> char {
    crate::util::test_data_charset_last_sorted()
}

/// A random lowercase token, re-exported for symmetry with upstream's imports.
pub fn token() -> String {
    random_token(10)
}

/// Every document generator this module defines, with its upstream line, for the
/// provenance self-check.
pub fn all_generators() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "humanData",
            "src/plugins/test-utils/schema-objects.ts:73",
            random_human_data(),
        ),
        (
            "simpleHumanData",
            "src/plugins/test-utils/schema-objects.ts:86",
            simple_human_data(),
        ),
        (
            "simpleHumanV3Data",
            "src/plugins/test-utils/schema-objects.ts:99",
            simple_human_v3_data(Map::new()),
        ),
        (
            "nestedHumanData",
            "src/plugins/test-utils/schema-objects.ts:159",
            nested_human_data(Map::new()),
        ),
        (
            "deepNestedHumanData",
            "src/plugins/test-utils/schema-objects.ts:184",
            deep_nested_human_data(),
        ),
        (
            "bigHumanDocumentType",
            "src/plugins/test-utils/schema-objects.ts:204",
            big_human_data(),
        ),
        (
            "heroArrayData",
            "src/plugins/test-utils/schema-objects.ts:221",
            hero_array_data(),
        ),
        (
            "simpleHeroArray",
            "src/plugins/test-utils/schema-objects.ts:237",
            simple_hero_array(Map::new()),
        ),
        (
            "averageSchemaData",
            "src/plugins/test-utils/schema-objects.ts:432",
            average_schema_data(Map::new()),
        ),
        (
            "pointData",
            "src/plugins/test-utils/schema-objects.ts:462",
            point_data(),
        ),
        (
            "humanWithSubOther",
            "src/plugins/test-utils/schema-objects.ts:131",
            human_with_sub_other(),
        ),
        (
            "NoIndexHuman",
            "src/plugins/test-utils/schema-objects.ts:144",
            no_index_human(),
        ),
    ]
}
