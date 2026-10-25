//! Query errors.
//!
//! Upstream throws `RxError` with a code. The codes a query can raise are
//! reproduced by name so a ported test can assert the same code it asserts
//! upstream, and so a caller can distinguish "the selector is not valid" from
//! "this schema cannot be planned".

use thiserror::Error;

/// The upstream error codes this crate can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryErrorCode {
    /// `QU12` — a custom `index` that is not one of the schema's indexes.
    Qu12,
    /// `QU13` — a selector or sort refers to a field the schema does not have.
    Qu13,
    /// `QU16` — a `RegExp` instance appears in a query.
    Qu16,
    /// `QU19` — a selector property is `undefined`.
    Qu19,
    /// `QU20` — a sort field uses bracket notation for an array index.
    Qu20,
    /// `SNH` — "should never happen": an internal invariant broke.
    Snh,
    /// `CI1` — an index field is not in the schema.
    Ci1,
    /// `CI2` — an index field's type cannot be encoded as an index string.
    Ci2,
    /// An operator outside the supported Mango set
    /// (`src/types/rx-query.d.ts:43`).
    UnknownOperator,
    /// A top-level key outside `$and`/`$or`/`$nor` that starts with `$`.
    UnknownTopLevelOperator,
    /// A `$regex` pattern that the ECMAScript grammar rejects.
    InvalidRegex,
    /// A malformed operator payload (for example `$mod` with three entries).
    InvalidOperatorPayload,
    /// A document failed to validate against the schema. Re-exported from
    /// `dxdb-schema` in spirit; kept here so query callers have one error type.
    Validation,
}

impl QueryErrorCode {
    /// The code as upstream spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qu12 => "QU12",
            Self::Qu13 => "QU13",
            Self::Qu16 => "QU16",
            Self::Qu19 => "QU19",
            Self::Qu20 => "QU20",
            Self::Snh => "SNH",
            Self::Ci1 => "CI1",
            Self::Ci2 => "CI2",
            Self::UnknownOperator => "QU21",
            Self::UnknownTopLevelOperator => "QU22",
            Self::InvalidRegex => "QU23",
            Self::InvalidOperatorPayload => "QU24",
            Self::Validation => "VD1",
        }
    }
}

/// A query error: an upstream code plus human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {message}", code = .code.as_str())]
pub struct QueryError {
    /// The upstream code.
    pub code: QueryErrorCode,
    /// What went wrong, in terms a caller can act on.
    pub message: String,
}

impl QueryError {
    /// Builds an error from a code and a message.
    pub fn new(code: QueryErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<dxdb_schema::SchemaError> for QueryError {
    fn from(error: dxdb_schema::SchemaError) -> Self {
        // The schema codes overlap the query codes only for the index builder
        // (CI1/CI2), which is why the mapping is explicit rather than a
        // pass-through of a `String`.
        let code = match error.code {
            dxdb_schema::SchemaErrorCode::Ci1 => QueryErrorCode::Ci1,
            dxdb_schema::SchemaErrorCode::Ci2 => QueryErrorCode::Ci2,
            _ => QueryErrorCode::Snh,
        };
        Self {
            code,
            message: error.message,
        }
    }
}
