//! Query-correctness fixture data. **Generated — do not edit.**
//!
//! Regenerate with `python3 crates/dxdb-testkit/gen/gen_correctness.py`;
//! verify with `--check`.
//!
//! One [`Group`] per upstream `testCorrectQueries` call. Each group carries
//! the schema it runs against, the documents it inserts, and every query with
//! its expected result ids — extracted from upstream rather than transcribed,
//! so a transposed id cannot pass review.
//!
//! # Why the values are JSON
//!
//! A group's data is `serde_json::Value`, parsed once from the extracted
//! literal. Typed structs per group would be 25 near-identical declarations
//! and would still have to carry the same JSON for the schema, which a storage
//! consumes as JSON anyway ([`dxdb_storage::StorageSchema::from_value`]).
//! Upstream's fixtures are object literals for the same reason.

use serde_json::Value;
use std::sync::OnceLock;

use serde_json::json;

use crate::fixtures::correctness_support::{Group, Query, SchemaSource};

/// Every group, in upstream order.
pub fn groups() -> &'static [Group] {
    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        vec![
            group_01(),
            group_02(),
            group_03(),
            group_04(),
            group_05(),
            group_06(),
            group_07(),
            group_08(),
            group_09(),
            group_10(),
            group_11(),
            group_12(),
            group_13(),
            group_14(),
            group_15(),
            group_16(),
            group_17(),
            group_18(),
            group_19(),
            group_20(),
            group_21(),
            group_22(),
            group_23(),
            group_24(),
            group_25(),
        ]
    })
}

/// `$gt/$gte`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:385`
fn group_01() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 1,
            title: r#"$gt/$gte"#,
            upstream_line: 385,
            primary_path: r#"passportId"#,
            schema: SchemaSource::WithIndexes {
                base: crate::schemas::human,
                base_name: r#"human"#,
                indexes: vec![json!(["age"]), json!(["age","firstName"]), json!(["firstName"]), json!(["passportId"])],
            },
            data: parse_list(&[
                r#"{"passportId":"aa","firstName":"alice","lastName":"last-aa","age":10}"#,
                r#"{"passportId":"bb","firstName":"bob","lastName":"last-bb","age":20}"#,
                r#"{"passportId":"cc-looong-id","firstName":"carol","lastName":"last-cc-looong-id","age":30}"#,
                r#"{"passportId":"dd","firstName":"dave","lastName":"last-dd","age":40}"#,
                r#"{"passportId":"ee","firstName":"eve","lastName":"last-ee","age":50}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"normal $gt by number"#,
                    query: parse(r#"{"selector":{"age":{"$gt":20}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"cc-looong-id"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"normal $gte"#,
                    query: parse(r#"{"selector":{"age":{"$gte":20}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"bb"#, r#"cc-looong-id"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gt on primary key"#,
                    query: parse(r#"{"selector":{"passportId":{"$gt":"dd"}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gt and $gte on same field"#,
                    query: parse(r#"{"selector":{"age":{"$gte":40,"$gt":19}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"sort by something that is not in the selector"#,
                    query: parse(r#"{"selector":{"age":{"$gt":20}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"cc-looong-id"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"with string comparison"#,
                    query: parse(r#"{"selector":{"firstName":{"$gt":"bob"}}}"#),
                    expected_result_doc_ids: &[r#"cc-looong-id"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"compare more than one field"#,
                    query: parse(r#"{"selector":{"age":{"$gt":20},"firstName":{"$gt":"a"}}}"#),
                    expected_result_doc_ids: &[r#"cc-looong-id"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(false),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.humanData("aa", 10, "alice")"#,
                r#"schemaObjects.humanData("bb", 20, "bob")"#,
                r#"schemaObjects.humanData("cc-looong-id", 30, "carol")"#,
                r#"schemaObjects.humanData("dd", 40, "dave")"#,
                r#"schemaObjects.humanData("ee", 50, "eve")"#,
            ],
        })
        .clone()
}

/// `$lt/$lte`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:526`
fn group_02() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 2,
            title: r#"$lt/$lte"#,
            upstream_line: 526,
            primary_path: r#"passportId"#,
            schema: SchemaSource::WithIndexes {
                base: crate::schemas::human,
                base_name: r#"human"#,
                indexes: vec![json!(["age"]), json!(["age","firstName"]), json!(["firstName"]), json!(["passportId"])],
            },
            data: parse_list(&[
                r#"{"passportId":"aa","firstName":"alice","lastName":"last-aa","age":10}"#,
                r#"{"passportId":"bb","firstName":"bob","lastName":"last-bb","age":20}"#,
                r#"{"passportId":"cc-looong-id","firstName":"carol","lastName":"last-cc-looong-id","age":30}"#,
                r#"{"passportId":"dd","firstName":"dave","lastName":"last-dd","age":40}"#,
                r#"{"passportId":"ee","firstName":"eve","lastName":"last-ee","age":50}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"normal $lt"#,
                    query: parse(r#"{"selector":{"age":{"$lt":40}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc-looong-id"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"normal $lte"#,
                    query: parse(r#"{"selector":{"age":{"$lte":40}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc-looong-id"#, r#"dd"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"sort by something that is not in the selector"#,
                    query: parse(r#"{"selector":{"age":{"$lt":40}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc-looong-id"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt on primaryKey"#,
                    query: parse(r#"{"selector":{"passportId":{"$lt":"bb"}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"compare more than one field"#,
                    query: parse(r#"{"selector":{"age":{"$lt":40},"firstName":{"$lt":"d"}}}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc-looong-id"#],
                    selector_satisfied_by_index: Some(false),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.humanData("aa", 10, "alice")"#,
                r#"schemaObjects.humanData("bb", 20, "bob")"#,
                r#"schemaObjects.humanData("cc-looong-id", 30, "carol")"#,
                r#"schemaObjects.humanData("dd", 40, "dave")"#,
                r#"schemaObjects.humanData("ee", 50, "eve")"#,
            ],
        })
        .clone()
}

/// `nested properties`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:637`
fn group_03() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 3,
            title: r#"nested properties"#,
            upstream_line: 637,
            primary_path: r#"passportId"#,
            schema: SchemaSource::WithIndexes {
                base: crate::schemas::nested_human,
                base_name: r#"nestedHuman"#,
                indexes: vec![json!(["mainSkill.level"]), json!(["mainSkill.name"])],
            },
            data: parse_list(&[
                r#"{"passportId":"aaa","firstName":"unset","mainSkill":{"level":6,"name":"zzz"}}"#,
                r#"{"passportId":"bbb","firstName":"unset","mainSkill":{"level":4,"name":"ttt"}}"#,
                r#"{"passportId":"ccc","firstName":"unset","mainSkill":{"level":3,"name":"ccc"}}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"sort by nested mainSkill.name"#,
                    query: parse(r#"{"selector":{},"sort":[{"mainSkill.name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"ccc"#, r#"bbb"#, r#"aaa"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.nestedHumanData({"passportId": "aaa", "mainSkill": {"level": 6, "name": "zzz"}})"#,
                r#"schemaObjects.nestedHumanData({"passportId": "bbb", "mainSkill": {"level": 4, "name": "ttt"}})"#,
                r#"schemaObjects.nestedHumanData({"passportId": "ccc", "mainSkill": {"level": 3, "name": "ccc"}})"#,
            ],
        })
        .clone()
}

/// `$or`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:682`
fn group_04() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 4,
            title: r#"$or"#,
            upstream_line: 682,
            primary_path: r#"passportId"#,
            schema: SchemaSource::WithIndexes {
                base: crate::schemas::human_minimal,
                base_name: r#"humanMinimal"#,
                indexes: vec![],
            },
            data: parse_list(&[
                r#"{"passportId":"aaa","age":20,"oneOptional":"A"}"#,
                r#"{"passportId":"bbb","age":20,"oneOptional":"B"}"#,
                r#"{"passportId":"ccc","age":20}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"match A or B"#,
                    query: parse(r#"{"selector":{"$or":[{"passportId":"aaa"},{"passportId":"bbb"}]},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aaa"#, r#"bbb"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"match with optional field"#,
                    query: parse(r#"{"selector":{"passportId":{"$eq":"ccc"},"$or":[{"oneOptional":{"$ne":"foobar1"}},{"oneOptional":{"$ne":"foobar2"}}]},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"ccc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"match non on non-existing optional field"#,
                    query: parse(r#"{"selector":{"passportId":{"$eq":"foobar"},"$or":[{"oneOptional":{"$ne":"foobar1"}},{"oneOptional":{"$ne":"foobar2"}}]},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.simpleHumanV3Data({"passportId": "aaa", "oneOptional": "A"})"#,
                r#"schemaObjects.simpleHumanV3Data({"passportId": "bbb", "oneOptional": "B"})"#,
                r#"schemaObjects.simpleHumanV3Data({"passportId": "ccc"})"#,
            ],
        })
        .clone()
}

/// `$in`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:772`
fn group_05() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 5,
            title: r#"$in"#,
            upstream_line: 772,
            primary_path: r#"passportId"#,
            schema: SchemaSource::Named(crate::schemas::human),
            data: parse_list(&[
                r#"{"passportId":"aa","firstName":"alice","lastName":"last-aa","age":10}"#,
                r#"{"passportId":"bb","firstName":"bob","lastName":"last-bb","age":20}"#,
                r#"{"passportId":"cc","firstName":"carol","lastName":"last-cc","age":30}"#,
                r#"{"passportId":"dd","firstName":"dave","lastName":"last-dd","age":40}"#,
                r#"{"passportId":"ee","firstName":"eve","lastName":"last-ee","age":50}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"get first"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["alice"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get by multiple"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["alice","bob"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get none matching"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["foobar","barfoo"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get by primary key"#,
                    query: parse(r#"{"selector":{"passportId":{"$in":["aa","cc","ee"]}}}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"cc"#, r#"ee"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.humanData("aa", 10, "alice")"#,
                r#"schemaObjects.humanData("bb", 20, "bob")"#,
                r#"schemaObjects.humanData("cc", 30, "carol")"#,
                r#"schemaObjects.humanData("dd", 40, "dave")"#,
                r#"schemaObjects.humanData("ee", 50, "eve")"#,
            ],
        })
        .clone()
}

/// `$in with index use`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:844`
fn group_06() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 6,
            title: r#"$in with index use"#,
            upstream_line: 844,
            primary_path: r#"passportId"#,
            schema: SchemaSource::WithIndexes {
                base: crate::schemas::human,
                base_name: r#"human"#,
                indexes: vec![json!(["firstName","age"])],
            },
            data: parse_list(&[
                r#"{"passportId":"aa","firstName":"aaron","lastName":"last-aa","age":3}"#,
                r#"{"passportId":"bb","firstName":"aaron","lastName":"last-bb","age":10}"#,
                r#"{"passportId":"cc","firstName":"carol","lastName":"last-cc","age":4}"#,
                r#"{"passportId":"dd","firstName":"dave","lastName":"last-dd","age":2}"#,
                r#"{"passportId":"ee","firstName":"jack","lastName":"last-ee","age":1}"#,
                r#"{"passportId":"ff","firstName":"zoe","lastName":"last-ff","age":30}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$in combined with range operator and desc sort"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["aaron","jack","carol"]},"age":{"$lt":5}},"sort":[{"age":"desc"},{"passportId":"asc"}],"index":["firstName","age"],"limit":50}"#),
                    expected_result_doc_ids: &[r#"cc"#, r#"aa"#, r#"ee"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$in values that span the whole index range"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["aaron","zoe"]}},"sort":[{"passportId":"asc"}],"index":["firstName","age"]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"ff"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$in with mixed value types must not use the min/max bounds"#,
                    query: parse(r#"{"selector":{"firstName":{"$in":["aaron",42]}},"sort":[{"passportId":"asc"}],"index":["firstName","age"]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#],
                    selector_satisfied_by_index: Some(false),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.humanData("aa", 3, "aaron")"#,
                r#"schemaObjects.humanData("bb", 10, "aaron")"#,
                r#"schemaObjects.humanData("cc", 4, "carol")"#,
                r#"schemaObjects.humanData("dd", 2, "dave")"#,
                r#"schemaObjects.humanData("ee", 1, "jack")"#,
                r#"schemaObjects.humanData("ff", 30, "zoe")"#,
            ],
        })
        .clone()
}

/// `$in for array fields named skills`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:924`
fn group_07() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 7,
            title: r#"$in for array fields named skills"#,
            upstream_line: 924,
            primary_path: r#"name"#,
            schema: SchemaSource::Named(crate::schemas::simple_array_hero),
            data: parse_list(&[
                r#"{"name":"alice","skills":["a","b","c"]}"#,
                r#"{"name":"bob","skills":["c","d","e"]}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"get first"#,
                    query: parse(r#"{"selector":{"skills":{"$in":["a"]}}}"#),
                    expected_result_doc_ids: &[r#"alice"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get by multiple"#,
                    query: parse(r#"{"selector":{"skills":{"$in":["c"]}}}"#),
                    expected_result_doc_ids: &[r#"alice"#, r#"bob"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get none matching"#,
                    query: parse(r#"{"selector":{"skills":{"$in":["aa"]}}}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &["", ""],
        })
        .clone()
}

/// `$nin`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:972`
fn group_08() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 8,
            title: r#"$nin"#,
            upstream_line: 972,
            primary_path: r#"passportId"#,
            schema: SchemaSource::Named(crate::schemas::human),
            data: parse_list(&[
                r#"{"passportId":"aa","firstName":"alice","lastName":"last-aa","age":10}"#,
                r#"{"passportId":"bb","firstName":"bob","lastName":"last-bb","age":20}"#,
                r#"{"passportId":"cc","firstName":"carol","lastName":"last-cc","age":30}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"get all but first"#,
                    query: parse(r#"{"selector":{"firstName":{"$nin":["alice"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"bb"#, r#"cc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get all but multiple"#,
                    query: parse(r#"{"selector":{"firstName":{"$nin":["alice","bob"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"cc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get all matching"#,
                    query: parse(r#"{"selector":{"firstName":{"$nin":["foobar","barfoo"]}},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"get by primary key"#,
                    query: parse(r#"{"selector":{"passportId":{"$nin":["aa","cc"]}}}"#),
                    expected_result_doc_ids: &[r#"bb"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                r#"schemaObjects.humanData("aa", 10, "alice")"#,
                r#"schemaObjects.humanData("bb", 20, "bob")"#,
                r#"schemaObjects.humanData("cc", 30, "carol")"#,
            ],
        })
        .clone()
}

/// `$elemMatch/$size`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1039`
fn group_09() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 9,
            title: r#"$elemMatch/$size"#,
            upstream_line: 1039,
            primary_path: r#"name"#,
            schema: SchemaSource::Named(crate::schemas::hero_array),
            data: parse_list(&[
                r#"{"name":"foo1","skills":[{"name":"bar1","damage":10},{"name":"bar2","damage":5}]}"#,
                r#"{"name":"foo2","skills":[{"name":"bar3","damage":10},{"name":"bar4","damage":10}]}"#,
                r#"{"name":"foo3","skills":[{"name":"bar5","damage":5}]}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$elemMatch"#,
                    query: parse(r#"{"selector":{"skills":{"$elemMatch":{"damage":5}}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"foo1"#, r#"foo3"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$elemMatch with other operator"#,
                    query: parse(r#"{"selector":{"name":{"$eq":"foo3"},"skills":{"$elemMatch":{"damage":5}}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"foo3"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$elemMatch with regex operator payload"#,
                    query: parse(r#"{"selector":{"skills":{"$elemMatch":{"name":{"$regex":"^bar[13]$","$options":"i"}}}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"foo1"#, r#"foo2"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$elemMatch with nested logical operator"#,
                    query: parse(r#"{"selector":{"skills":{"$elemMatch":{"$or":[{"damage":5},{"name":{"$regex":"^bar4$"}}]}}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"foo1"#, r#"foo2"#, r#"foo3"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"$size"#,
                    query: parse(r#"{"selector":{"skills":{"$size":1}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"foo3"#],
                    selector_satisfied_by_index: Some(false),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `$elemMatch nested arrays with regex`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1184`
fn group_10() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 10,
            title: r#"$elemMatch nested arrays with regex"#,
            upstream_line: 1184,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":100},"groups":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"tags":{"type":"array","items":{"type":"string"}}}}}},"required":["id","groups"]}"#),
            data: parse_list(&[
                r#"{"id":"a","groups":[{"name":"admins","tags":["Owner","Ops"]},{"name":"users","tags":["read"]}]}"#,
                r#"{"id":"b","groups":[{"name":"guests","tags":["ops"]}]}"#,
                r#"{"id":"c","groups":[{"name":"admins","tags":["audit"]}]}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$elemMatch on normal object array fields"#,
                    query: parse(r#"{"selector":{"groups":{"$elemMatch":{"name":"admins"}}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"a"#, r#"c"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"nested $elemMatch with regex/$options payload"#,
                    query: parse(r#"{"selector":{"groups":{"$elemMatch":{"tags":{"$elemMatch":{"$regex":"^ops$","$options":"i"}}}}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"a"#, r#"b"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `$eq operator`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1290`
fn group_11() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 11,
            title: r#"$eq operator"#,
            upstream_line: 1290,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":100},"nonPrimaryString":{"type":"string"},"integer":{"type":"integer"},"number":{"type":"number"},"boolean":{"type":"boolean"},"null":{"type":["null","string"]}},"indexes":["boolean"],"required":["id","nonPrimaryString","integer","number","boolean"]}"#),
            data: parse_list(&[
                r#"{"id":"zero","nonPrimaryString":"zero","integer":0,"number":0,"boolean":false,"null":"not-null"}"#,
                r#"{"id":"one","nonPrimaryString":"one","integer":1,"number":1,"boolean":true,"null":null}"#,
                r#"{"id":"two","nonPrimaryString":"two","integer":2,"number":2,"boolean":false,"null":"not-null"}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$eq primary key"#,
                    query: parse(r#"{"selector":{"id":{"$eq":"one"}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq non-primary string"#,
                    query: parse(r#"{"selector":{"nonPrimaryString":{"$eq":"one"}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq integer"#,
                    query: parse(r#"{"selector":{"integer":{"$eq":1}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq number"#,
                    query: parse(r#"{"selector":{"number":{"$eq":1}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq boolean"#,
                    query: parse(r#"{"selector":{"boolean":{"$eq":true}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq null"#,
                    query: parse(r#"{"selector":{"null":{"$eq":null}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$or with $eq null"#,
                    query: parse(r#"{"selector":{"$or":[{"null":null},{"null":"not-null","id":"two"}]},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one"#, r#"two"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `$eq operator with composite primary key`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1466`
fn group_12() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 12,
            title: r#"$eq operator with composite primary key"#,
            upstream_line: 1466,
            primary_path: r#"key"#,
            schema: SchemaSource::Inline(r#"{"version":0,"indexes":["string",["number","integer"]],"primaryKey":{"key":"key","fields":["string","number","integer"],"separator":"|"},"type":"object","properties":{"key":{"maxLength":100,"type":"string"},"id":{"maxLength":100,"type":"string"},"string":{"maxLength":50,"type":"string"},"number":{"type":"number","minimum":0,"maximum":100,"multipleOf":1},"integer":{"type":"integer","minimum":0,"maximum":100,"multipleOf":1}},"required":["id","key","string","number","integer"]}"#),
            data: parse_list(&[
                r#"{"id":"one","key":"one|1|1","string":"one","number":1,"integer":1}"#,
                r#"{"id":"two","key":"two|1|1","string":"two","number":1,"integer":1}"#,
                r#"{"id":"three","key":"one|2|1","string":"one","number":2,"integer":1}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$eq primary key 2"#,
                    query: parse(r#"{"selector":{"id":{"$eq":"one"}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one|1|1"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq by key"#,
                    query: parse(r#"{"selector":{"key":{"$eq":"one|1|1"}},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"one|1|1"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$eq by composite key fields"#,
                    query: parse(r#"{"selector":{"$and":[{"string":{"$eq":"one"}},{"number":{"$eq":1},"integer":{"$eq":1}}]},"sort":[{"number":"desc","integer":"desc"}]}"#),
                    expected_result_doc_ids: &[r#"one|1|1"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `issue: compound index has wrong range`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1587`
fn group_13() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 13,
            title: r#"issue: compound index has wrong range"#,
            upstream_line: 1587,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"indexes":[["exists","hasHighlights","lastOpenedAt"]],"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":1},"hasHighlights":{"type":"integer","minimum":0,"maximum":1,"multipleOf":1},"lastOpenedAt":{"type":"integer","minimum":0,"maximum":9007199254740991,"multipleOf":1},"exists":{"type":"integer","minimum":0,"maximum":1,"multipleOf":1}},"required":["id","hasHighlights","lastOpenedAt","exists"]}"#),
            data: parse_list(&[
                r#"{"id":"1","exists":1,"hasHighlights":1,"lastOpenedAt":1600000000000}"#,
                r#"{"id":"2","exists":1,"hasHighlights":1,"lastOpenedAt":1700000000000}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"multiple operators"#,
                    query: parse(r#"{"selector":{"exists":1,"lastOpenedAt":{"$gte":1600000000000,"$lte":1650000000000}}}"#),
                    expected_result_doc_ids: &[r#"1"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"multiple operators 2"#,
                    query: parse(r#"{"selector":{"exists":1,"lastOpenedAt":{"$gte":1600000000000}}}"#),
                    expected_result_doc_ids: &[r#"1"#, r#"2"#],
                    selector_satisfied_by_index: Some(false),
                },
                Query {
                    info: r#"all operators in index"#,
                    query: parse(r#"{"selector":{"exists":1,"hasHighlights":1,"lastOpenedAt":{"$gte":1600000000000}}}"#),
                    expected_result_doc_ids: &[r#"1"#, r#"2"#],
                    selector_satisfied_by_index: Some(true),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
            ],
        })
        .clone()
}

/// `$type`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1680`
fn group_14() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 14,
            title: r#"$type"#,
            upstream_line: 1680,
            primary_path: r#"foo"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"foo","type":"object","properties":{"foo":{"type":"string","maxLength":100},"bar":{"oneOf":[{"type":"string"},{"type":"number"}]}},"required":["foo","bar"]}"#),
            data: parse_list(&[
                r#"{"foo":"1","bar":"test"}"#,
                r#"{"foo":"2","bar":2.0}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$type string"#,
                    query: parse(r#"{"selector":{"bar":{"$type":"string"}},"sort":[{"foo":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"1"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$type number"#,
                    query: parse(r#"{"selector":{"bar":{"$type":"number"}},"sort":[{"foo":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"2"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
            ],
        })
        .clone()
}

/// `issue: wrong results on complex index`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1750`
fn group_15() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 15,
            title: r#"issue: wrong results on complex index"#,
            upstream_line: 1750,
            primary_path: r#"_id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"_id","type":"object","version":0,"properties":{"_id":{"type":"string","maxLength":20},"name":{"type":"string","maxLength":20},"gender":{"type":"string","enum":["f","m","x"],"maxLength":1},"age":{"type":"number","minimum":0,"maximum":100,"multipleOf":1}},"required":["_id","name","gender","age"],"indexes":[["name","gender","age","_id"],["gender","age","name","_id"],["age","name","gender","_id"]]}"#),
            data: parse_list(&[
                r#"{"_id":"nogljngyvo","name":"cjbovwbzjx","gender":"f","age":18}"#,
                r#"{"_id":"zmbznyggnu","name":"rpjljekeoy","gender":"m","age":3}"#,
                r#"{"_id":"hauezldqea","name":"ckjndqrthh","gender":"f","age":20}"#,
                r#"{"_id":"utarwoqkav","name":"thfubuvqwr","gender":"m","age":12}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"complex query on index"#,
                    query: parse(r#"{"selector":{"gender":{"$gt":"x"},"name":{"$lt":"hqybnsozrv"}},"sort":[{"gender":"asc"},{"age":"asc"},{"_id":"asc"}],"index":["name","gender","age","_id"]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"complex query on end of index"#,
                    query: parse(r#"{"selector":{"gender":{"$lt":"x","$lte":"m"}},"sort":[{"age":"asc"},{"name":"asc"},{"_id":"asc"}],"index":["gender","age","name","_id"]}"#),
                    expected_result_doc_ids: &[r#"zmbznyggnu"#, r#"utarwoqkav"#, r#"nogljngyvo"#, r#"hauezldqea"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"had wrong index string on upper bound"#,
                    query: parse(r#"{"selector":{"age":{"$gte":4,"$lte":20},"gender":{"$lt":"m"}},"sort":[{"name":"asc"},{"_id":"asc"}],"index":["age","name","gender","_id"]}"#),
                    expected_result_doc_ids: &[r#"nogljngyvo"#, r#"hauezldqea"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"had wrong index string on upper bound for $eq"#,
                    query: parse(r#"{"selector":{"age":{"$lte":12},"gender":{"$lt":"x","$eq":"m"}},"sort":[{"_id":"asc"}],"index":["gender","age","name","_id"]}"#),
                    expected_result_doc_ids: &[r#"utarwoqkav"#, r#"zmbznyggnu"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `issue: wrong results using skip and limit`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:1957`
fn group_16() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 16,
            title: r#"issue: wrong results using skip and limit"#,
            upstream_line: 1957,
            primary_path: r#"_id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"_id","type":"object","version":0,"properties":{"_id":{"type":"string","maxLength":20},"name":{"type":"string","maxLength":20},"gender":{"type":"string","enum":["f","m","x"],"maxLength":1},"age":{"type":"number","minimum":0,"maximum":100,"multipleOf":1}},"indexes":[["_id"]]}"#),
            data: parse_list(&[
                r#"{"_id":"aaaaaa","name":"odvxvubzto","gender":"f","age":7}"#,
                r#"{"_id":"bbbbbb","name":"ftudnsnyek","gender":"f","age":14}"#,
                r#"{"_id":"cccccc","name":"ollytrnxkr","gender":"f","age":3}"#,
                r#"{"_id":"dddddd","name":"sxzsplrctw","gender":"f","age":15}"#,
                r#"{"_id":"eeeeee","name":"gwhtwjinib","gender":"m","age":9}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"use skip + limit, expecting no results"#,
                    query: parse(r#"{"selector":{"gender":{"$eq":"f"},"name":{"$gt":"baavgagzmr","$lte":"mzahchkldk"}},"skip":1,"limit":5,"sort":[{"gender":"asc"},{"_id":"asc"}],"index":["_id"]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"use skip + limit, expecting one specific result"#,
                    query: parse(r#"{"selector":{"age":{"$gt":3},"name":{"$gt":"enjpqcusiu","$lt":"ircrnmjhkd"}},"skip":1,"limit":9,"sort":[{"gender":"asc"},{"_id":"asc"}],"index":["_id"]}"#),
                    expected_result_doc_ids: &[r#"eeeeee"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `issue: Inserting without an optional index field causes unexpected behavior`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2087`
fn group_17() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 17,
            title: r#"issue: Inserting without an optional index field causes unexpected behavior"#,
            upstream_line: 2087,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"id","type":"object","version":0,"properties":{"id":{"type":"string","maxLength":20},"numberIndex":{"type":"number","minimum":1,"maximum":40,"multipleOf":1}},"indexes":["numberIndex"]}"#),
            data: parse_list(&[
                r#"{"id":"aa","numberIndex":2}"#,
                r#"{"id":"bb"}"#,
                r#"{"id":"cc","numberIndex":5}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"without selector"#,
                    query: parse(r#"{"selector":{},"sort":[{"id":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"without selector and numberIndex"#,
                    query: parse(r#"{"selector":{},"sort":[{"numberIndex":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"bb"#, r#"aa"#, r#"cc"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: true,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `Using undefined in a selector must have the same result in all storages`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2156`
fn group_18() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 18,
            title: r#"Using undefined in a selector must have the same result in all storages"#,
            upstream_line: 2156,
            primary_path: r#"passportId"#,
            schema: SchemaSource::Named(crate::schemas::human_minimal),
            data: parse_list(&[
                r#"{"passportId":"there","oneOptional":"opt","age":10}"#,
                r#"{"passportId":"not-there","age":10}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"oneOptional is null"#,
                    query: parse(
                        r#"{"selector":{"oneOptional":null},"sort":[{"passportId":"asc"}]}"#,
                    ),
                    expected_result_doc_ids: &[r#"not-there"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"oneOptional does not exist in selector"#,
                    query: parse(r#"{"selector":{},"sort":[{"passportId":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"not-there"#, r#"there"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"oneOptional is empty object"#,
                    query: parse(
                        r#"{"selector":{"oneOptional":{}},"sort":[{"passportId":"asc"}]}"#,
                    ),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &["", ""],
        })
        .clone()
}

/// `number comparison at exact boundary values including min/max`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2217`
fn group_19() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 19,
            title: r#"number comparison at exact boundary values including min/max"#,
            upstream_line: 2217,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"id","type":"object","version":0,"properties":{"id":{"type":"string","maxLength":20},"score":{"type":"number","minimum":0,"maximum":100,"multipleOf":1}},"required":["id","score"],"indexes":["score"]}"#),
            data: parse_list(&[
                r#"{"id":"aa","score":0}"#,
                r#"{"id":"bb","score":1}"#,
                r#"{"id":"cc","score":50}"#,
                r#"{"id":"dd","score":99}"#,
                r#"{"id":"ee","score":100}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$gt at the minimum stored value excludes it"#,
                    query: parse(r#"{"selector":{"score":{"$gt":0}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gte at the minimum stored value includes it"#,
                    query: parse(r#"{"selector":{"score":{"$gte":0}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt at the maximum stored value excludes it"#,
                    query: parse(r#"{"selector":{"score":{"$lt":100}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lte at the maximum stored value includes it"#,
                    query: parse(r#"{"selector":{"score":{"$lte":100}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gt at an intermediate value excludes it"#,
                    query: parse(r#"{"selector":{"score":{"$gt":50}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt at an intermediate value excludes it"#,
                    query: parse(r#"{"selector":{"score":{"$lt":50}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"single document range with $gte + $lte at same value"#,
                    query: parse(r#"{"selector":{"score":{"$gte":50,"$lte":50}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"cc"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"empty range with $gt + $lt at same value"#,
                    query: parse(r#"{"selector":{"score":{"$gt":50,"$lt":50}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$gt at the maximum stored value returns empty"#,
                    query: parse(r#"{"selector":{"score":{"$gt":100}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt at the minimum stored value returns empty"#,
                    query: parse(r#"{"selector":{"score":{"$lt":0}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[],
                    selector_satisfied_by_index: Some(true),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `negative decimal numbers sort and query correctly`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2332`
fn group_20() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 20,
            title: r#"negative decimal numbers sort and query correctly"#,
            upstream_line: 2332,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"id","type":"object","version":0,"properties":{"id":{"type":"string","maxLength":20},"temperature":{"type":"number","minimum":-100,"maximum":100,"multipleOf":0.01}},"required":["id","temperature"],"indexes":["temperature"]}"#),
            data: parse_list(&[
                r#"{"id":"a","temperature":-1.9}"#,
                r#"{"id":"b","temperature":-1.5}"#,
                r#"{"id":"c","temperature":-1.1}"#,
                r#"{"id":"d","temperature":-1.0}"#,
                r#"{"id":"e","temperature":0.0}"#,
                r#"{"id":"f","temperature":1.1}"#,
                r#"{"id":"g","temperature":1.5}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"sort ascending returns negative decimals in correct order"#,
                    query: parse(r#"{"selector":{"temperature":{"$gte":-2}},"sort":[{"temperature":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"a"#, r#"b"#, r#"c"#, r#"d"#, r#"e"#, r#"f"#, r#"g"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gt on a negative decimal excludes smaller values"#,
                    query: parse(r#"{"selector":{"temperature":{"$gt":-1.5}},"sort":[{"temperature":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"c"#, r#"d"#, r#"e"#, r#"f"#, r#"g"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt on a negative decimal excludes larger values"#,
                    query: parse(r#"{"selector":{"temperature":{"$lt":-1.0}},"sort":[{"temperature":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"a"#, r#"b"#, r#"c"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"range query with negative decimal bounds"#,
                    query: parse(r#"{"selector":{"temperature":{"$gte":-1.5,"$lte":-1.1}},"sort":[{"temperature":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"b"#, r#"c"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `compound index with decimal numbers preserves correct sort order`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2396`
fn group_21() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 21,
            title: r#"compound index with decimal numbers preserves correct sort order"#,
            upstream_line: 2396,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"primaryKey":"id","type":"object","version":0,"properties":{"id":{"type":"string","maxLength":20},"score":{"type":"number","minimum":-10,"maximum":10,"multipleOf":0.1}},"required":["id","score"],"indexes":["score"]}"#),
            data: parse_list(&[
                r#"{"id":"a","score":0}"#,
                r#"{"id":"b","score":1}"#,
                r#"{"id":"c","score":2}"#,
                r#"{"id":"d","score":3}"#,
                r#"{"id":"e","score":4}"#,
                r#"{"id":"f","score":5}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$gt on integer value with decimal-precision index"#,
                    query: parse(r#"{"selector":{"score":{"$gt":2}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"d"#, r#"e"#, r#"f"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$lt on integer value with decimal-precision index"#,
                    query: parse(r#"{"selector":{"score":{"$lt":3}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"a"#, r#"b"#, r#"c"#],
                    selector_satisfied_by_index: Some(true),
                },
                Query {
                    info: r#"$gte/$lte range with decimal-precision index"#,
                    query: parse(r#"{"selector":{"score":{"$gte":1,"$lte":4}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"b"#, r#"c"#, r#"d"#, r#"e"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `enum field with $eq should be sort-irrelevant and return correct results`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2451`
fn group_22() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 22,
            title: r#"enum field with $eq should be sort-irrelevant and return correct results"#,
            upstream_line: 2451,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":100},"status":{"type":"string","enum":["active","inactive","pending"],"maxLength":20},"age":{"type":"integer","minimum":0,"maximum":150,"multipleOf":1}},"indexes":[["status","age"]],"required":["id","status","age"]}"#),
            data: parse_list(&[
                r#"{"id":"1","status":"active","age":30}"#,
                r#"{"id":"2","status":"inactive","age":10}"#,
                r#"{"id":"3","status":"active","age":10}"#,
                r#"{"id":"4","status":"active","age":50}"#,
                r#"{"id":"5","status":"pending","age":20}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"enum $eq with sort on next index field returns correct results in order"#,
                    query: parse(r#"{"selector":{"status":{"$eq":"active"}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"3"#, r#"1"#, r#"4"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `$gt/$lt on field before boolean index field should not include boundary docs`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2506`
fn group_23() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 23,
            title: r#"$gt/$lt on field before boolean index field should not include boundary docs"#,
            upstream_line: 2506,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":100},"age":{"type":"integer","minimum":0,"maximum":150,"multipleOf":1},"isActive":{"type":"boolean"}},"indexes":[["age","isActive"]],"required":["id","age","isActive"]}"#),
            data: parse_list(&[
                r#"{"id":"a","age":10,"isActive":true}"#,
                r#"{"id":"b","age":30,"isActive":true}"#,
                r#"{"id":"c","age":30,"isActive":false}"#,
                r#"{"id":"d","age":50,"isActive":false}"#,
                r#"{"id":"e","age":50,"isActive":true}"#,
                r#"{"id":"f","age":70,"isActive":false}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$gt/$lt exclusive bounds must not include boundary age values"#,
                    query: parse(r#"{"selector":{"age":{"$gt":20,"$lt":50}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"b"#, r#"c"#],
                    selector_satisfied_by_index: Some(true),
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `string fields with control characters (codepoint < 32) must be found by queries`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2555`
fn group_24() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 24,
            title: r#"string fields with control characters (codepoint < 32) must be found by queries"#,
            upstream_line: 2555,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":100},"name":{"type":"string","maxLength":100},"age":{"type":"integer","minimum":0,"maximum":150,"multipleOf":1}},"indexes":[["name"],["age","name"]],"required":["id","name","age"]}"#),
            data: parse_list(&[
                r#"{"id":"tab-name","name":"\tAlice","age":30}"#,
                r#"{"id":"newline-name","name":"\nBob","age":25}"#,
                r#"{"id":"normal-name","name":"Carol","age":35}"#,
                r#"{"id":"space-name","name":" Dave","age":40}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"find all documents including those with control characters in string fields"#,
                    query: parse(r#"{"selector":{},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"tab-name"#, r#"newline-name"#, r#"space-name"#, r#"normal-name"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"control character strings with $gte should be found"#,
                    query: parse(r#"{"selector":{"name":{"$gte":"\t"}},"sort":[{"name":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"tab-name"#, r#"newline-name"#, r#"space-name"#, r#"normal-name"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"compound index: $gt on first field must not leak docs via control chars in second field"#,
                    query: parse(r#"{"selector":{"age":{"$gt":30}},"sort":[{"age":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"normal-name"#, r#"space-name"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// `range bound outside the schema minimum/maximum must still match the boundary documents`
///
/// Upstream: `test/unit/rx-storage-query-correctness.test.ts:2621`
fn group_25() -> Group {
    static CACHE: OnceLock<Group> = OnceLock::new();
    CACHE
        .get_or_init(|| Group {
            index: 25,
            title: r#"range bound outside the schema minimum/maximum must still match the boundary documents"#,
            upstream_line: 2621,
            primary_path: r#"id"#,
            schema: SchemaSource::Inline(r#"{"version":0,"indexes":[["score"]],"primaryKey":"id","type":"object","properties":{"id":{"type":"string","maxLength":2},"score":{"type":"number","minimum":0,"maximum":100,"multipleOf":1}},"required":["id","score"]}"#),
            data: parse_list(&[
                r#"{"id":"aa","score":0}"#,
                r#"{"id":"bb","score":1}"#,
                r#"{"id":"cc","score":50}"#,
                r#"{"id":"dd","score":99}"#,
                r#"{"id":"ee","score":100}"#,
            ]),
            queries: vec![
                Query {
                    info: r#"$gt below the schema minimum must include the minimum document"#,
                    query: parse(r#"{"selector":{"score":{"$gt":-10}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$lt above the schema maximum must include the maximum document"#,
                    query: parse(r#"{"selector":{"score":{"$lt":110}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$gt at the exact minimum must still exclude it"#,
                    query: parse(r#"{"selector":{"score":{"$gt":0}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"bb"#, r#"cc"#, r#"dd"#, r#"ee"#],
                    selector_satisfied_by_index: None,
                },
                Query {
                    info: r#"$lt at the exact maximum must still exclude it"#,
                    query: parse(r#"{"selector":{"score":{"$lt":100}},"sort":[{"score":"asc"}]}"#),
                    expected_result_doc_ids: &[r#"aa"#, r#"bb"#, r#"cc"#, r#"dd"#],
                    selector_satisfied_by_index: None,
                },
            ],
            not_run_if_dexie: false,
            // The upstream calls that produced `data`, one per document,
            // or an empty string where upstream wrote the document literal out.
            recipes: &[
                "",
                "",
                "",
                "",
                "",
            ],
        })
        .clone()
}

/// Parses one JSON literal from the generated data.
#[track_caller]
fn parse(text: &str) -> Value {
    serde_json::from_str(text)
        .unwrap_or_else(|err| panic!("generated JSON is malformed ({err}): {text}"))
}

/// Parses a list of JSON literals.
#[track_caller]
fn parse_list(texts: &[&str]) -> Vec<Value> {
    texts.iter().map(|text| parse(text)).collect()
}
