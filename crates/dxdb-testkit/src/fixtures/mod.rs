//! Fixtures: the schemas, documents and query data the conformance suites run on.
//!
//! Everything here is a port of upstream's `src/plugins/test-utils/`, so a ported
//! case reads like the original. Modules:
//!
//! | module | upstream |
//! | --- | --- |
//! | [`schemas`](crate::schemas) | `plugins/test-utils/schemas.ts` |
//! | [`objects`](crate::fixtures::objects) | `plugins/test-utils/schema-objects.ts` |
//! | [`revisions`](crate::fixtures::objects) constants | `plugins/test-utils/revisions.ts` |
//! | [`write`] | the `getWriteData` / `Object.assign` idiom the suites use |
//! | [`correctness`] | the 25 `testCorrectQueries` groups, extracted |
//!
//! # Generated data
//!
//! [`correctness`] and its support types are generated from upstream by
//! `gen/gen_correctness.py`. The values are extracted rather than transcribed,
//! because a hand-copied expected-id list still compiles and still passes against a
//! permissive storage — the failure mode this whole crate exists to prevent.

pub mod correctness;
pub mod correctness_support;
pub mod objects;
pub mod write;

pub use correctness_support::{document_count, query_count, Group, Query, SchemaSource};
pub use objects::{
    EXAMPLE_REVISION_1, EXAMPLE_REVISION_2, EXAMPLE_REVISION_3, EXAMPLE_REVISION_4,
    EXAMPLE_REVISIONS,
};
pub use write::{
    attachments_with, attachment_stub, delete_row, doc, field, field_path, insert_row, primary_of,
    test_doc_write_data, update_row, with_fields, write, write_data, write_data_lwt,
    write_data_with,
};
