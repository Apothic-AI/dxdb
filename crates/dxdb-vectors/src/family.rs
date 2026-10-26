//! One module per vector family.
//!
//! Each module mirrors the JSON the generator writes for that family, field for
//! field. Where the data is itself arbitrary JSON produced by JavaScript (a query
//! selector, a stored document, a defaulted schema), it is kept as
//! [`serde_json::Value`] with a comment naming what produced it. Inventing a typed
//! model for those would be a second, hand-written interpretation of the very data
//! the vectors exist to check; the shape that matters is the family's own envelope,
//! and that is what is typed.

pub mod bulk_write_categorization;
pub mod index_strings;
pub mod jsnum;
pub mod normalised_queries;
pub mod replication_transcripts;
pub mod schema_hashes;
pub mod selector_matching;
